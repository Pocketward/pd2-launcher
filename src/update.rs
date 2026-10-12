//! Verified PD2 update and next-start repair workflow.
//!
//! Update availability is advisory domain state; applying an update is an
//! explicit, mutually exclusive mutation.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fs::File,
    io::{self, Read, Seek, Write},
    path::Path,
    time::Instant,
};

#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::os::unix::fs::MetadataExt;

use md5::{Digest as _, Md5};

#[cfg(test)]
use crate::manifest::hex;
use crate::{
    audit::Md5Digest,
    manifest::{ManifestError, Pd2Manifest},
    network::{NetworkError, ObjectRequest, ObjectResponse, Pd2Source, RetryPolicy},
    operation::{OperationControl, ProgressUnit, WorkPhase, WorkProgress},
    safe_fs::Directory,
    transaction::{
        ExclusiveLease, FaultInjector, NoFaults, Promotion, TransactionError, TransactionManager,
        UpdateLayout,
    },
};

#[cfg(test)]
use crate::manifest::ManifestResponse;

const MAX_OBJECT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ACCEPTED_MANIFEST_BYTES: u64 = 1024 * 1024;
const PROTECTED_FILES: [&str; 10] = [
    "D2.LNG",
    "BnetLog.txt",
    "ProjectDiablo.cfg",
    "ddraw.ini",
    "default.filter",
    "loot.filter",
    "UI.ini",
    "d2gl.yaml",
    "d2gl.json",
    "PD2Shared.dll",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GameActivity {
    Idle,
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtectedInventoryEvidence {
    Complete,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApplyOutcome {
    NoOp,
    Applied {
        downloaded: usize,
        promoted: usize,
        preserved: usize,
    },
}

pub(crate) struct VerifiedUpdater<'a> {
    layout: UpdateLayout,
    source: &'a mut dyn Pd2Source,
    faults: &'a dyn FaultInjector,
    retry_policy: RetryPolicy,
}

impl<'a> VerifiedUpdater<'a> {
    #[cfg(test)]
    pub(crate) fn new(
        project_root: impl Into<std::path::PathBuf>,
        source: &'a mut dyn Pd2Source,
    ) -> Self {
        Self::with_layout(UpdateLayout::new(project_root), source)
    }

    pub(crate) fn with_layout(layout: UpdateLayout, source: &'a mut dyn Pd2Source) -> Self {
        Self {
            layout,
            source,
            faults: &NoFaults,
            retry_policy: default_retry_policy(),
        }
    }

    #[cfg(test)]
    fn with_faults(mut self, faults: &'a dyn FaultInjector) -> Self {
        self.faults = faults;
        self
    }

    /// Performs the read-only automatic check. It never stages or promotes.
    pub(crate) fn check(&mut self) -> Result<Pd2Manifest, UpdateError> {
        fetch_manifest(self.source, None, self.retry_policy)
    }

    /// Applies the current manifest only after explicit user intent has reached
    /// this workflow with idle-game and complete-inventory evidence.
    #[cfg(test)]
    pub(crate) fn apply(
        &mut self,
        transaction_id: &str,
        game: GameActivity,
        inventory: ProtectedInventoryEvidence,
    ) -> Result<ApplyOutcome, UpdateError> {
        self.apply_controlled(
            transaction_id,
            game,
            inventory,
            &OperationControl::default(),
        )
    }

    pub(crate) fn apply_controlled(
        &mut self,
        transaction_id: &str,
        game: GameActivity,
        inventory: ProtectedInventoryEvidence,
        control: &OperationControl,
    ) -> Result<ApplyOutcome, UpdateError> {
        if game == GameActivity::Running {
            return Err(UpdateError::GameRunning);
        }
        if inventory == ProtectedInventoryEvidence::Indeterminate {
            return Err(UpdateError::InventoryIndeterminate);
        }

        reject_cancelled(control)?;
        control.report(WorkProgress::for_phase(WorkPhase::ClientManifest));
        let manifest = fetch_manifest(self.source, Some(control), self.retry_policy)?;
        reject_cancelled(control)?;
        manifest.accepted()?;
        let lease = self.layout.acquire_exclusive()?;
        let local = inspect_local_client(lease.project_directory(), &manifest)?;
        let plan = plan(&manifest, &local)?;
        if plan.promotions.is_empty() && accepted_is(lease.metadata_directory(), &manifest)? {
            return Ok(ApplyOutcome::NoOp);
        }
        self.layout
            .prepare_staging_with_lease(transaction_id, &manifest, &lease)?;
        let pre_journal = (|| {
            let mut downloaded = 0;
            for (index, promotion) in plan.promotions.iter().enumerate() {
                reject_cancelled(control)?;
                let entry = manifest
                    .entries()
                    .iter()
                    .find(|entry| entry.path() == &promotion.path)
                    .ok_or(UpdateError::UnsafePlan)?;
                if download_entry(
                    self.source,
                    DownloadEntry {
                        lease: &lease,
                        manifest: &manifest,
                        entry,
                        overall_completed: index as u64,
                        overall_total: plan.promotions.len().max(1) as u64,
                        retry_policy: self.retry_policy,
                        control,
                    },
                )? {
                    downloaded += 1;
                }
                lease.refresh_staging_slot_identity(manifest.id(), entry.path().as_str())?;
                control.report(
                    WorkProgress::for_phase(WorkPhase::ClientDownload)
                        .with_overall(
                            ProgressUnit::Files,
                            (index + 1) as u64,
                            Some(plan.promotions.len().max(1) as u64),
                        )
                        .with_verification(
                            (index + 1) as u64,
                            Some(plan.promotions.len().max(1) as u64),
                        ),
                );
            }
            self.layout
                .mark_staging_ready_for_journal(transaction_id, &manifest, &lease)?;
            let promotions = plan
                .promotions
                .iter()
                .map(|entry| Promotion {
                    path: entry.path.clone(),
                    expected_md5: entry.digest.bytes(),
                })
                .collect::<Vec<_>>();
            reject_cancelled(control)?;
            Ok::<_, UpdateError>((downloaded, promotions))
        })();
        let (downloaded, promotions) = match pre_journal {
            Ok(result) => result,
            Err(error) => {
                if self
                    .layout
                    .discard_unjournaled_staging(&manifest, &lease)
                    .is_err()
                {
                    return Err(UpdateError::Indeterminate);
                }
                return Err(error);
            }
        };
        control.report(WorkProgress::bounded(
            WorkPhase::Promotion,
            ProgressUnit::Files,
            0,
            promotions.len().max(1) as u64,
        ));
        TransactionManager::new(&self.layout, self.faults).promote_with_lease_controlled(
            transaction_id,
            &manifest,
            &promotions,
            &lease,
            control,
        )?;

        Ok(ApplyOutcome::Applied {
            downloaded,
            promoted: promotions.len(),
            preserved: plan.preserved,
        })
    }
}

fn default_retry_policy() -> RetryPolicy {
    #[cfg(test)]
    {
        RetryPolicy::immediate_for_tests()
    }
    #[cfg(not(test))]
    {
        RetryPolicy::standard()
    }
}

#[derive(Clone, Debug)]
struct PlannedPromotion {
    path: crate::RelativeGamePath,
    digest: Md5Digest,
}

#[derive(Clone, Debug)]
struct UpdatePlan {
    promotions: Vec<PlannedPromotion>,
    preserved: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalFileEvidence {
    Matching,
    MissingOrChanged,
    ProtectedPresent,
}

#[derive(Clone, Debug)]
struct LocalClientEvidence(BTreeMap<crate::RelativeGamePath, LocalFileEvidence>);

fn fetch_manifest(
    source: &mut dyn Pd2Source,
    control: Option<&OperationControl>,
    retry_policy: RetryPolicy,
) -> Result<Pd2Manifest, UpdateError> {
    let response = fetch_manifest_attempts(source, control, retry_policy)?;
    Ok(Pd2Manifest::parse_from(response, &source.client_base())?)
}

fn fetch_manifest_response(
    source: &mut dyn Pd2Source,
    control: &OperationControl,
    retry_policy: RetryPolicy,
) -> Result<crate::manifest::ManifestResponse, UpdateError> {
    fetch_manifest_attempts(source, Some(control), retry_policy)
}

fn fetch_manifest_attempts(
    source: &mut dyn Pd2Source,
    control: Option<&OperationControl>,
    retry_policy: RetryPolicy,
) -> Result<crate::manifest::ManifestResponse, UpdateError> {
    let started = Instant::now();
    let mut response = None;
    let mut final_error = None;
    for attempt in 0..retry_policy.max_attempts() {
        let result = match control {
            Some(control) => source.fetch_manifest_controlled(control),
            None => source.fetch_manifest(),
        };
        match result {
            Ok(value) => {
                response = Some(value);
                break;
            }
            Err(_) if control.is_some_and(OperationControl::is_cancelled) => {
                return Err(UpdateError::Cancelled);
            }
            Err(error) if error.is_transient() => {
                let retry_after = error.retry_after();
                final_error = Some(error);
                if !retry_policy.pause(attempt, retry_after, started, control) {
                    break;
                }
            }
            Err(error) => return Err(UpdateError::Network(error)),
        }
    }
    let response = response.ok_or_else(|| {
        UpdateError::Network(
            final_error
                .unwrap_or_else(|| NetworkError::transient("manifest retry budget exhausted")),
        )
    })?;
    if control.is_some_and(OperationControl::is_cancelled) {
        return Err(UpdateError::Cancelled);
    }
    Ok(response)
}

fn inspect_local_client(
    project: &Directory,
    manifest: &Pd2Manifest,
) -> Result<LocalClientEvidence, UpdateError> {
    let mut files = BTreeMap::new();
    for entry in manifest.entries() {
        let name = std::ffi::OsStr::new(entry.path().as_str());
        let evidence =
            if is_protected(entry.path().as_str()) && open_optional(project, name)?.is_some() {
                LocalFileEvidence::ProtectedPresent
            } else if local_digest_at(project, name)?
                .is_some_and(|digest| digest == entry.digest().bytes())
            {
                LocalFileEvidence::Matching
            } else {
                LocalFileEvidence::MissingOrChanged
            };
        files.insert(entry.path().clone(), evidence);
    }
    Ok(LocalClientEvidence(files))
}

fn plan(manifest: &Pd2Manifest, local: &LocalClientEvidence) -> Result<UpdatePlan, UpdateError> {
    let mut promotions = Vec::new();
    let mut preserved = 0;
    for entry in manifest.entries() {
        match local.0.get(entry.path()).ok_or(UpdateError::UnsafePlan)? {
            LocalFileEvidence::Matching => continue,
            LocalFileEvidence::ProtectedPresent => {
                preserved += 1;
                continue;
            }
            LocalFileEvidence::MissingOrChanged => {}
        }
        promotions.push(PlannedPromotion {
            path: entry.path().clone(),
            digest: entry.digest(),
        });
    }
    Ok(UpdatePlan {
        promotions,
        preserved,
    })
}

fn accepted_is(metadata: &Directory, manifest: &Pd2Manifest) -> Result<bool, UpdateError> {
    let Some(file) = open_optional(metadata, std::ffi::OsStr::new("accepted-manifest.json"))?
    else {
        return Ok(false);
    };
    if file.metadata()?.len() > MAX_ACCEPTED_MANIFEST_BYTES {
        return Err(UpdateError::UnsafePath);
    }
    let mut bytes = Vec::new();
    file.take(MAX_ACCEPTED_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ACCEPTED_MANIFEST_BYTES {
        return Err(UpdateError::UnsafePath);
    }
    Ok(ShaIdentity::of(&bytes) == ShaIdentity::of(manifest.raw()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShaIdentity([u8; 32]);

impl ShaIdentity {
    fn of(bytes: &[u8]) -> Self {
        use sha2::{Digest as _, Sha256};
        Self(Sha256::digest(bytes).into())
    }
}

struct DownloadEntry<'a> {
    lease: &'a ExclusiveLease,
    manifest: &'a Pd2Manifest,
    entry: &'a crate::manifest::ManifestEntry,
    overall_completed: u64,
    overall_total: u64,
    retry_policy: RetryPolicy,
    control: &'a OperationControl,
}

fn download_entry(
    source: &mut dyn Pd2Source,
    download: DownloadEntry<'_>,
) -> Result<bool, UpdateError> {
    let DownloadEntry {
        lease,
        manifest,
        entry,
        overall_completed,
        overall_total,
        retry_policy,
        control,
    } = download;
    let manifest_staging = lease.manifest_staging(manifest.id())?;
    let partial_directory = manifest_staging.open_directory(Path::new(".partial"))?;
    let staged_directory = manifest_staging.open_directory(Path::new("files"))?;
    let (staged_slot, partial_slot) = lease.staging_slots(manifest.id(), entry.path().as_str())?;
    let staged_name = std::ffi::OsStr::new(&staged_slot);
    if local_digest_at(&staged_directory, staged_name)?
        .is_some_and(|digest| digest == entry.digest().bytes())
    {
        return Ok(false);
    }
    open_optional(&staged_directory, staged_name)?;

    let partial_name = std::ffi::OsStr::new(&partial_slot);
    let mut partial_file = partial_directory.open_or_create_file(partial_name)?;

    reject_cancelled(control)?;
    let copied = match source.local_object(entry.object_url()) {
        Some(local) => {
            copy_local_object(&local, &mut partial_file)?;
            let verified = digest_open_file(&mut partial_file)? == entry.digest().bytes();
            if !verified {
                partial_directory.truncate_open_file(partial_name, &partial_file)?;
            }
            verified
        }
        None => false,
    };
    if !copied {
        fetch_verified(
            source,
            FetchObject {
                url: entry.object_url(),
                accept_redirects: manifest.is_custom_source(),
                expected: entry.digest().bytes(),
                overall_completed,
                overall_total,
                retry_policy,
                control,
            },
            &partial_directory,
            partial_name,
            &mut partial_file,
        )?;
    }
    partial_directory.require_open_file(partial_name, &partial_file)?;
    staged_directory.atomic_write_from_open(staged_name, &partial_file)?;
    partial_directory.truncate_open_file(partial_name, &partial_file)?;
    Ok(true)
}

/// Copy a verified local object (a cached download) into `destination`,
/// sharing its blocks (`FICLONE`) where the filesystem supports reflinks
/// and copying in the kernel otherwise. The source is never moved or
/// changed, and the caller verifies the copy's MD5.
fn copy_local_object(source: &File, destination: &mut File) -> Result<(), UpdateError> {
    if source.metadata()?.len() > MAX_OBJECT_BYTES {
        return Err(UpdateError::ObjectTooLarge);
    }
    destination.set_len(0)?;
    if rustix::fs::ioctl_ficlone(&*destination, source).is_err() {
        destination.set_len(0)?;
        destination.rewind()?;
        let mut reader = source.try_clone()?;
        reader.rewind()?;
        io::copy(&mut reader.take(MAX_OBJECT_BYTES + 1), destination)?;
    }
    destination.sync_all()?;
    if destination.metadata()?.len() > MAX_OBJECT_BYTES {
        return Err(UpdateError::ObjectTooLarge);
    }
    Ok(())
}

struct FetchObject<'a> {
    url: &'a str,
    accept_redirects: bool,
    expected: [u8; 16],
    overall_completed: u64,
    overall_total: u64,
    retry_policy: RetryPolicy,
    control: &'a OperationControl,
}

/// Download one object into `partial_file`, resuming what it already holds,
/// until its MD5 matches the manifest. A mismatch empties the partial.
fn fetch_verified(
    source: &mut dyn Pd2Source,
    fetch: FetchObject<'_>,
    partial_directory: &Directory,
    partial_name: &std::ffi::OsStr,
    partial_file: &mut File,
) -> Result<(), UpdateError> {
    let FetchObject {
        url,
        accept_redirects,
        expected,
        overall_completed,
        overall_total,
        retry_policy,
        control,
    } = fetch;
    let mut last_transient = false;
    let started = Instant::now();
    for attempt in 0..retry_policy.max_attempts() {
        reject_cancelled(control)?;
        let range_start = partial_file.metadata()?.len();
        let request = ObjectRequest {
            url: url.to_owned(),
            range_start: (range_start > 0).then_some(range_start),
            accept_redirects,
        };
        let response = match source.open_object_controlled(&request, control) {
            Ok(response) => response,
            Err(error) if error.is_transient() => {
                last_transient = true;
                if !retry_policy.pause(attempt, error.retry_after(), started, Some(control)) {
                    break;
                }
                continue;
            }
            Err(error) => return Err(UpdateError::Network(error)),
        };
        match append_response(
            partial_file,
            &request,
            response,
            overall_completed,
            overall_total,
            control,
        ) {
            Ok(()) => {}
            Err(_) if control.is_cancelled() => return Err(UpdateError::Cancelled),
            Err(UpdateError::DownloadInterrupted) => {
                last_transient = true;
                if !retry_policy.pause(attempt, None, started, Some(control)) {
                    break;
                }
                continue;
            }
            Err(UpdateError::RetryableResponse(retry_after)) => {
                last_transient = true;
                if !retry_policy.pause(attempt, retry_after, started, Some(control)) {
                    break;
                }
                continue;
            }
            Err(error) => return Err(error),
        }
        control.report(
            WorkProgress::for_phase(WorkPhase::Hashing)
                .with_file_bytes(partial_file.metadata()?.len(), None)
                .with_overall(ProgressUnit::Files, overall_completed, Some(overall_total))
                .with_verification(overall_completed + 1, Some(overall_total)),
        );
        let digest = digest_open_file(partial_file)?;
        if digest != expected {
            partial_directory.truncate_open_file(partial_name, partial_file)?;
            return Err(UpdateError::DigestMismatch);
        }
        return Ok(());
    }
    if last_transient {
        Err(UpdateError::RetryExhausted)
    } else {
        Err(UpdateError::DownloadInterrupted)
    }
}

/// The prefetched manifest, where it came from, and its verified objects,
/// below the prefetch cache.
const PREFETCH_MANIFEST: &str = "manifest.json";
const PREFETCH_MANIFEST_SOURCE: &str = "manifest.source.json";
const PREFETCH_OBJECTS: &str = "objects";
const PREFETCH_PARTIAL_SUFFIX: &str = ".partial";

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PrefetchedManifestSource {
    requested_url: String,
    final_url: String,
}

fn md5_hex(bytes: [u8; 16]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Download every object of the current manifest into `cache` and verify
/// each against its manifest MD5, installing nothing (the Download step).
/// Objects already there and still matching are kept; anything
/// the manifest no longer names is removed, so the cache holds exactly one
/// client. A missing object is copied from `installed` (a ProjectD2) when
/// the file there has the manifest's MD5, and downloaded otherwise. The
/// manifest is written last, and only when it changed. Only the cache
/// changes and a partial object resumes, so a stop anywhere here is proven
/// safe.
pub(crate) fn prefetch_client(
    source: &mut dyn Pd2Source,
    cache: &Directory,
    installed: Option<&Directory>,
    control: &OperationControl,
) -> Result<(), UpdateError> {
    prefetch_client_files(source, cache, installed, control).inspect_err(|error| {
        if matches!(error, UpdateError::Cancelled) && control.is_cancelled() {
            control.mark_cancelled_safe();
        }
    })
}

fn prefetch_client_files(
    source: &mut dyn Pd2Source,
    cache: &Directory,
    installed: Option<&Directory>,
    control: &OperationControl,
) -> Result<(), UpdateError> {
    reject_cancelled(control)?;
    control.report(WorkProgress::for_phase(WorkPhase::ClientManifest));
    let retry_policy = default_retry_policy();
    let response = fetch_manifest_response(source, control, retry_policy)?;
    let manifest = Pd2Manifest::parse_from(response.clone(), &source.client_base())?;
    manifest.accepted()?;
    let objects = cache.ensure_directory(Path::new(PREFETCH_OBJECTS))?;
    let mut wanted = Vec::new();
    for entry in manifest.entries() {
        let digest = entry.digest().bytes();
        if !wanted.iter().any(|(known, _)| *known == digest) {
            wanted.push((digest, entry));
        }
    }
    for name in objects.names()? {
        let keep = name.to_str().is_some_and(|name| {
            let object = name.strip_suffix(PREFETCH_PARTIAL_SUFFIX).unwrap_or(name);
            wanted.iter().any(|(digest, _)| md5_hex(*digest) == object)
        });
        if !keep {
            let file = objects
                .open_file(Path::new(&name))
                .map_err(|_| UpdateError::UnsafePath)?;
            objects.remove_open_file_bounded(&name, &file)?;
        }
    }
    let total = wanted.len() as u64;
    for (index, (digest, entry)) in wanted.iter().enumerate() {
        reject_cancelled(control)?;
        let name = md5_hex(*digest);
        let object_name = std::ffi::OsStr::new(&name);
        if local_digest_at(&objects, object_name)?.is_some_and(|local| local == *digest) {
            continue;
        }
        let partial = format!("{name}{PREFETCH_PARTIAL_SUFFIX}");
        let partial_name = std::ffi::OsStr::new(&partial);
        let mut partial_file = objects.open_or_create_file(partial_name)?;
        let copied = match installed_copy(installed, &manifest, *digest)? {
            Some(local) => {
                copy_local_object(&local, &mut partial_file)?;
                let verified = digest_open_file(&mut partial_file)? == *digest;
                if !verified {
                    objects.truncate_open_file(partial_name, &partial_file)?;
                }
                verified
            }
            None => false,
        };
        if !copied {
            fetch_verified(
                source,
                FetchObject {
                    url: entry.object_url(),
                    accept_redirects: manifest.is_custom_source(),
                    expected: *digest,
                    overall_completed: index as u64,
                    overall_total: total,
                    retry_policy,
                    control,
                },
                &objects,
                partial_name,
                &mut partial_file,
            )?;
        }
        objects.require_open_file(partial_name, &partial_file)?;
        objects.atomic_write_from_open(object_name, &partial_file)?;
        objects.remove_open_file_bounded(partial_name, &partial_file)?;
        control.report(
            WorkProgress::for_phase(WorkPhase::ClientDownload)
                .with_overall(ProgressUnit::Files, index as u64 + 1, Some(total))
                .with_verification(index as u64 + 1, Some(total)),
        );
    }
    let origin = serde_json::to_vec(&PrefetchedManifestSource {
        requested_url: response.requested_url,
        final_url: response.final_url,
    })
    .map_err(|_| UpdateError::UnsafePath)?;
    write_if_changed(cache, PREFETCH_MANIFEST_SOURCE, &origin)?;
    write_if_changed(cache, PREFETCH_MANIFEST, manifest.raw())?;
    Ok(())
}

/// An installed file holding the object `digest`, found by the manifest
/// paths that name it. Opened below `installed` without following links,
/// and hashed; the copy made from it is hashed again.
fn installed_copy(
    installed: Option<&Directory>,
    manifest: &Pd2Manifest,
    digest: [u8; 16],
) -> Result<Option<File>, UpdateError> {
    let Some(installed) = installed else {
        return Ok(None);
    };
    for entry in manifest.entries() {
        if entry.digest().bytes() != digest {
            continue;
        }
        let name = std::ffi::OsStr::new(entry.path().as_str());
        // A file that can't be opened safely is simply not reused.
        let Ok(Some(mut file)) = open_optional(installed, name) else {
            continue;
        };
        if file.metadata()?.len() <= MAX_OBJECT_BYTES && digest_open_file(&mut file)? == digest {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Rewrite a small cache file only when its bytes change, so an unchanged
/// manifest keeps the identity the Download record holds.
fn write_if_changed(cache: &Directory, name: &str, bytes: &[u8]) -> Result<(), UpdateError> {
    let name = std::ffi::OsStr::new(name);
    if let Some(file) = open_optional(cache, name)? {
        let mut current = Vec::new();
        file.take(bytes.len() as u64 + 1)
            .read_to_end(&mut current)?;
        if current == bytes {
            return Ok(());
        }
    }
    cache.atomic_write(name, bytes)?;
    Ok(())
}

/// A PD2 source that serves the objects the Download step verified from
/// its cache, and everything else (objects of a manifest that moved on
/// since) from `inner`. An object is served from the cache only when its
/// MD5 is the one the manifest just fetched names, and the updater verifies
/// every object's MD5 again, wherever it came from. When the manifest can't
/// be fetched, the prefetched one is used, so installing needs no network.
pub(crate) struct PrefetchedPd2Source<'a> {
    inner: &'a mut dyn Pd2Source,
    objects: Option<Directory>,
    by_url: BTreeMap<String, [u8; 16]>,
    manifest: Option<crate::manifest::ManifestResponse>,
}

impl<'a> PrefetchedPd2Source<'a> {
    pub(crate) fn new(inner: &'a mut dyn Pd2Source, cache: Option<Directory>) -> Self {
        let mut source = Self {
            inner,
            objects: None,
            by_url: BTreeMap::new(),
            manifest: None,
        };
        let Some(cache) = cache else {
            return source;
        };
        let read = |name: &str| -> Option<Vec<u8>> {
            let file = open_optional(&cache, std::ffi::OsStr::new(name)).ok()??;
            let mut bytes = Vec::new();
            file.take(MAX_ACCEPTED_MANIFEST_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            (bytes.len() as u64 <= MAX_ACCEPTED_MANIFEST_BYTES).then_some(bytes)
        };
        let (Some(raw), Some(origin)) = (read(PREFETCH_MANIFEST), read(PREFETCH_MANIFEST_SOURCE))
        else {
            return source;
        };
        let Ok(origin) = serde_json::from_slice::<PrefetchedManifestSource>(&origin) else {
            return source;
        };
        let response = crate::manifest::ManifestResponse {
            requested_url: origin.requested_url,
            final_url: origin.final_url,
            declared_length: Some(raw.len() as u64),
            body: raw,
        };
        if Pd2Manifest::parse_from(response.clone(), &source.inner.client_base()).is_err() {
            return source;
        }
        source.objects = cache.open_directory(Path::new(PREFETCH_OBJECTS)).ok();
        source.manifest = Some(response);
        source
    }

    /// Serve cached objects by the digests of the manifest being applied.
    fn follow(&mut self, response: &crate::manifest::ManifestResponse) {
        self.by_url = Pd2Manifest::parse_from(response.clone(), &self.inner.client_base())
            .map(|manifest| {
                manifest
                    .entries()
                    .iter()
                    .map(|entry| (entry.object_url().to_owned(), entry.digest().bytes()))
                    .collect()
            })
            .unwrap_or_default();
    }

    fn prefetched(&self, url: &str) -> Option<File> {
        let digest = self.by_url.get(url)?;
        let objects = self.objects.as_ref()?;
        let name = md5_hex(*digest);
        let mut file = open_optional(objects, std::ffi::OsStr::new(&name)).ok()??;
        if digest_open_file(&mut file).ok()? != *digest {
            return None;
        }
        file.rewind().ok()?;
        Some(file)
    }
}

impl Pd2Source for PrefetchedPd2Source<'_> {
    fn client_base(&self) -> crate::manifest::Pd2ClientBase {
        self.inner.client_base()
    }

    fn fetch_manifest(&mut self) -> Result<crate::manifest::ManifestResponse, NetworkError> {
        let response = match self.inner.fetch_manifest() {
            Ok(response) => response,
            Err(error) => self.manifest.clone().ok_or(error)?,
        };
        self.follow(&response);
        Ok(response)
    }

    fn fetch_manifest_controlled(
        &mut self,
        control: &OperationControl,
    ) -> Result<crate::manifest::ManifestResponse, NetworkError> {
        let response = match self.inner.fetch_manifest_controlled(control) {
            Ok(response) => response,
            Err(error) if control.is_cancelled() => return Err(error),
            Err(error) => self.manifest.clone().ok_or(error)?,
        };
        self.follow(&response);
        Ok(response)
    }

    fn open_object(&mut self, request: &ObjectRequest) -> Result<ObjectResponse, NetworkError> {
        self.inner.open_object(request)
    }

    /// Cached objects are copied, never moved: the cache stays whole for
    /// Start over and the next Download.
    fn local_object(&mut self, url: &str) -> Option<File> {
        self.prefetched(url)
    }

    fn open_object_controlled(
        &mut self,
        request: &ObjectRequest,
        control: &OperationControl,
    ) -> Result<ObjectResponse, NetworkError> {
        self.inner.open_object_controlled(request, control)
    }
}

fn append_response(
    output: &mut File,
    request: &ObjectRequest,
    response: ObjectResponse,
    overall_completed: u64,
    overall_total: u64,
    control: &OperationControl,
) -> Result<(), UpdateError> {
    if !request.accept_redirects && response.final_url != request.url {
        return Err(UpdateError::OriginRejected);
    }
    let restart = match (request.range_start, response.status) {
        (None, 200) => false,
        (Some(_), 200) => true,
        (Some(start), 206) => {
            let range = response.content_range.ok_or(UpdateError::InvalidRange)?;
            if range.start != start
                || range.end_inclusive < range.start
                || range.end_inclusive >= range.complete_length
            {
                return Err(UpdateError::InvalidRange);
            }
            let segment_length = range.end_inclusive - range.start + 1;
            if response
                .declared_length
                .is_some_and(|length| length != segment_length)
            {
                return Err(UpdateError::DownloadInterrupted);
            }
            false
        }
        (_, 429 | 500..=599) => {
            return Err(UpdateError::RetryableResponse(response.retry_after));
        }
        _ => return Err(UpdateError::UnexpectedStatus),
    };
    let existing = if restart {
        0
    } else {
        request.range_start.unwrap_or(0)
    };
    if existing > MAX_OBJECT_BYTES
        || response
            .declared_length
            .is_some_and(|length| length > MAX_OBJECT_BYTES.saturating_sub(existing))
        || response
            .content_range
            .is_some_and(|range| range.complete_length > MAX_OBJECT_BYTES)
    {
        return Err(UpdateError::ObjectTooLarge);
    }

    if restart || request.range_start.is_none() {
        output.set_len(0)?;
        output.seek(io::SeekFrom::Start(0))?;
    } else {
        output.seek(io::SeekFrom::End(0))?;
    }
    let remaining = MAX_OBJECT_BYTES.saturating_sub(if restart { 0 } else { existing });
    let mut bounded = response.body.take(remaining + 1);
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        reject_cancelled(control)?;
        let read = bounded
            .read(&mut buffer)
            .map_err(|_| UpdateError::DownloadInterrupted)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        copied = copied
            .checked_add(read as u64)
            .ok_or(UpdateError::ObjectTooLarge)?;
        let completed = existing.saturating_add(copied);
        let total = response
            .content_range
            .map(|range| range.complete_length)
            .or_else(|| {
                response
                    .declared_length
                    .map(|length| existing.saturating_add(length))
            });
        control.report(
            WorkProgress::for_phase(WorkPhase::ClientDownload)
                .with_file_bytes(completed, total)
                .with_overall(ProgressUnit::Files, overall_completed, Some(overall_total))
                .with_verification(overall_completed, Some(overall_total)),
        );
    }
    output.flush()?;
    output.sync_all()?;
    if response
        .declared_length
        .is_some_and(|length| length != copied)
    {
        return Err(UpdateError::DownloadInterrupted);
    }
    if output.metadata()?.len() > MAX_OBJECT_BYTES {
        return Err(UpdateError::ObjectTooLarge);
    }
    if let Some(range) = response.content_range
        && output.metadata()?.len() != range.complete_length
    {
        return Err(UpdateError::DownloadInterrupted);
    }
    Ok(())
}

#[cfg(test)]
fn open_partial(path: &Path) -> Result<File, UpdateError> {
    let file = match rustix::fs::open(
        path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(error) if error == rustix::io::Errno::NOENT => File::from(
            rustix::fs::open(
                path,
                rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(|error| UpdateError::Io(error.into()))?,
        ),
        Err(error) => return Err(UpdateError::Io(error.into())),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(UpdateError::UnsafePath);
    }
    Ok(file)
}

#[cfg(test)]
fn ensure_path_is_open_file(path: &Path, file: &File) -> Result<(), UpdateError> {
    let path_metadata = fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.dev() != file_metadata.dev()
        || path_metadata.ino() != file_metadata.ino()
        || path_metadata.nlink() != 1
    {
        return Err(UpdateError::UnsafePath);
    }
    Ok(())
}

fn digest_open_file(file: &mut File) -> Result<[u8; 16], UpdateError> {
    file.seek(io::SeekFrom::Start(0))?;
    let mut hasher = Md5::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn reject_cancelled(control: &OperationControl) -> Result<(), UpdateError> {
    if control.is_cancelled() {
        Err(UpdateError::Cancelled)
    } else {
        Ok(())
    }
}

fn open_optional(
    directory: &Directory,
    name: &std::ffi::OsStr,
) -> Result<Option<File>, UpdateError> {
    match directory.open_file(Path::new(name)) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(UpdateError::UnsafePath),
    }
}

fn local_digest_at(
    directory: &Directory,
    name: &std::ffi::OsStr,
) -> Result<Option<[u8; 16]>, UpdateError> {
    let Some(mut file) = open_optional(directory, name)? else {
        return Ok(None);
    };
    digest_open_file(&mut file).map(Some)
}

/// User-owned files, and files the official PD2 launcher replaces itself,
/// are preserved when present rather than overwritten.
fn is_protected(path: &str) -> bool {
    PROTECTED_FILES.contains(&path) || crate::audit::is_official_launcher_file(path)
}

#[derive(Debug)]
pub(crate) enum UpdateError {
    Cancelled,
    Indeterminate,
    GameRunning,
    InventoryIndeterminate,
    OriginRejected,
    UnexpectedStatus,
    InvalidRange,
    DownloadInterrupted,
    RetryExhausted,
    RetryableResponse(Option<std::time::Duration>),
    DigestMismatch,
    ObjectTooLarge,
    UnsafePath,
    UnsafePlan,
    Manifest(ManifestError),
    Network(NetworkError),
    Transaction(TransactionError),
    Io(io::Error),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Network(error) => write!(formatter, "{error}"),
            Self::Transaction(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "verified update I/O failed: {error}"),
            other => write!(formatter, "verified update failed: {other:?}"),
        }
    }
}

impl Error for UpdateError {}

impl From<ManifestError> for UpdateError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<NetworkError> for UpdateError {
    fn from(error: NetworkError) -> Self {
        Self::Network(error)
    }
}

impl From<TransactionError> for UpdateError {
    fn from(error: TransactionError) -> Self {
        Self::Transaction(error)
    }
}

impl From<io::Error> for UpdateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Cursor,
        os::unix::fs::symlink,
        path::PathBuf,
        sync::{Arc, mpsc::SyncSender},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        manifest::{PD2_MANIFEST_URL, PD2_ORIGIN},
        network::{ContentRange, NetworkError},
    };

    #[derive(Clone, Copy, Debug, Default)]
    enum ObjectBehavior {
        #[default]
        Normal,
        MissingLength,
        IgnoreRange,
        Redirect,
        WrongDigest,
        TruncateOnce,
    }

    struct FakeSource {
        manifest: Vec<u8>,
        objects: BTreeMap<String, Vec<u8>>,
        requests: Vec<ObjectRequest>,
        manifest_fetches: usize,
        behavior: ObjectBehavior,
        transient_failures: usize,
        truncated: bool,
        custom_base: Option<String>,
    }

    impl FakeSource {
        /// A source serving a user-supplied PD2 client base.
        fn with_custom_base(files: &[(&str, &[u8])], base: &str) -> Self {
            let mut source = Self::new(files);
            source.objects = files
                .iter()
                .map(|(name, bytes)| {
                    (
                        format!("{base}/{}", name.replace(' ', "%20")),
                        bytes.to_vec(),
                    )
                })
                .collect();
            source.custom_base = Some(base.to_owned());
            source
        }

        fn new(files: &[(&str, &[u8])]) -> Self {
            let manifest = manifest_body(files);
            let objects = files
                .iter()
                .map(|(name, bytes)| (object_url(name), bytes.to_vec()))
                .collect();
            Self {
                manifest,
                objects,
                requests: Vec::new(),
                manifest_fetches: 0,
                behavior: ObjectBehavior::Normal,
                transient_failures: 0,
                truncated: false,
                custom_base: None,
            }
        }
    }

    impl Pd2Source for FakeSource {
        fn client_base(&self) -> crate::manifest::Pd2ClientBase {
            self.custom_base.clone().map_or(
                crate::manifest::Pd2ClientBase::BuiltIn,
                crate::manifest::Pd2ClientBase::Custom,
            )
        }

        fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
            self.manifest_fetches += 1;
            let manifest_url = self.client_base().manifest_url();
            Ok(ManifestResponse {
                requested_url: manifest_url.clone(),
                final_url: manifest_url,
                declared_length: Some(self.manifest.len() as u64),
                body: self.manifest.clone(),
            })
        }

        fn open_object(&mut self, request: &ObjectRequest) -> Result<ObjectResponse, NetworkError> {
            self.requests.push(request.clone());
            if self.transient_failures > 0 {
                self.transient_failures -= 1;
                return Err(NetworkError::transient("temporary fake-server failure"));
            }
            let full = self
                .objects
                .get(&request.url)
                .cloned()
                .ok_or_else(|| NetworkError::permanent("unknown fake object"))?;
            let (status, body, content_range) = match (request.range_start, self.behavior) {
                (Some(_), ObjectBehavior::IgnoreRange) => (200, full.clone(), None),
                (Some(start), _) => {
                    let body = full[start as usize..].to_vec();
                    let range = ContentRange {
                        start,
                        end_inclusive: full.len() as u64 - 1,
                        complete_length: full.len() as u64,
                    };
                    (206, body, Some(range))
                }
                (None, ObjectBehavior::TruncateOnce) if !self.truncated => {
                    self.truncated = true;
                    (200, full[..full.len() / 2].to_vec(), None)
                }
                _ => (200, full.clone(), None),
            };
            let body = if matches!(self.behavior, ObjectBehavior::WrongDigest) {
                vec![b'x'; body.len().max(1)]
            } else {
                body
            };
            let declared_length = if matches!(self.behavior, ObjectBehavior::MissingLength) {
                None
            } else if matches!(self.behavior, ObjectBehavior::TruncateOnce)
                && self.truncated
                && request.range_start.is_none()
                && body.len() < full.len()
            {
                Some(full.len() as u64)
            } else {
                Some(body.len() as u64)
            };
            Ok(ObjectResponse {
                final_url: if matches!(self.behavior, ObjectBehavior::Redirect) {
                    "https://example.test/object".to_owned()
                } else {
                    request.url.clone()
                },
                status,
                declared_length,
                content_range,
                retry_after: None,
                body: Box::new(Cursor::new(body)),
            })
        }
    }

    #[test]
    fn prefetched_client_files_install_later_and_only_changed_ones_are_fetched() {
        let cache_path = TempDir::new().unwrap();
        let cache = Directory::open(cache_path.path()).unwrap();
        let mut source =
            FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]);
        prefetch_client(&mut source, &cache, None, &OperationControl::default()).unwrap();
        assert_eq!(source.requests.len(), 2);
        let objects = cache_path.path().join(PREFETCH_OBJECTS);
        let names = || {
            fs::read_dir(&objects)
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect::<Vec<_>>()
        };
        assert_eq!(names().len(), 2);
        let downloaded = cache_objects(cache_path.path());
        assert!(cache_path.path().join(PREFETCH_MANIFEST).is_file());
        // A second Download finds everything verified already.
        prefetch_client(&mut source, &cache, None, &OperationControl::default()).unwrap();
        assert_eq!(source.requests.len(), 2);

        // Installing serves every object from the cache: no download.
        let project = TempDir::new().unwrap();
        {
            let mut cached =
                PrefetchedPd2Source::new(&mut source, Some(cache.try_clone().unwrap()));
            let outcome = VerifiedUpdater::new(project.path(), &mut cached)
                .apply(
                    "txn-prefetched",
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                )
                .unwrap();
            assert!(matches!(outcome, ApplyOutcome::Applied { promoted: 2, .. }));
        }
        assert_eq!(source.requests.len(), 2, "nothing was downloaded again");
        assert_eq!(
            fs::read(project.path().join("Game.exe")).unwrap(),
            b"new game"
        );
        assert_eq!(
            cache_objects(cache_path.path()),
            downloaded,
            "installing copies each cached object and leaves the cache whole"
        );

        // The manifest moved on since the Download: only the changed file
        // is fetched; the unchanged one still comes from the cache.
        let mut moved = FakeSource::new(&[
            ("Game.exe", b"newer game"),
            ("ProjectDiablo.dll", b"new dll"),
        ]);
        let project = TempDir::new().unwrap();
        {
            let mut cached = PrefetchedPd2Source::new(&mut moved, Some(cache.try_clone().unwrap()));
            VerifiedUpdater::new(project.path(), &mut cached)
                .apply(
                    "txn-moved-on",
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                )
                .unwrap();
        }
        assert_eq!(
            moved
                .requests
                .iter()
                .map(|request| request.url.as_str())
                .collect::<Vec<_>>(),
            [object_url("Game.exe").as_str()]
        );
        assert_eq!(
            fs::read(project.path().join("Game.exe")).unwrap(),
            b"newer game"
        );

        // Offline: the prefetched manifest stands in for the network one.
        struct Offline;
        impl Pd2Source for Offline {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                Err(NetworkError::transient("offline"))
            }
            fn open_object(
                &mut self,
                _request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                Err(NetworkError::transient("offline"))
            }
        }
        let project = TempDir::new().unwrap();
        let mut offline = Offline;
        let mut cached = PrefetchedPd2Source::new(&mut offline, Some(cache));
        VerifiedUpdater::new(project.path(), &mut cached)
            .apply(
                "txn-offline",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();
        assert_eq!(
            fs::read(project.path().join("ProjectDiablo.dll")).unwrap(),
            b"new dll"
        );
    }

    #[test]
    fn a_stopped_prefetch_keeps_what_verified_and_never_publishes_a_bad_object() {
        let cache_path = TempDir::new().unwrap();
        let cache = Directory::open(cache_path.path()).unwrap();
        let mut source =
            FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]);
        let control = OperationControl::default();
        control.request_cancel();
        assert!(matches!(
            prefetch_client(&mut source, &cache, None, &control),
            Err(UpdateError::Cancelled)
        ));
        assert_eq!(
            control.cancellation_safety(),
            crate::operation::CancellationSafety::ProvenSafe
        );
        assert!(!cache_path.path().join(PREFETCH_MANIFEST).exists());
        source.behavior = ObjectBehavior::WrongDigest;
        assert!(prefetch_client(&mut source, &cache, None, &OperationControl::default()).is_err());
        let objects = cache_path.path().join(PREFETCH_OBJECTS);
        assert!(
            fs::read_dir(&objects)
                .unwrap()
                .flatten()
                .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
                .all(
                    |entry| entry.file_name().to_string_lossy().ends_with(".partial")
                        && entry.metadata().unwrap().len() == 0
                ),
            "no unverified object is kept"
        );
        assert!(!cache_path.path().join(PREFETCH_MANIFEST).exists());
    }

    #[test]
    fn changed_update_verifies_promotes_and_preserves_owned_and_unknown_files() {
        let temporary = TempDir::new().unwrap();
        fs::write(temporary.path().join("loot.filter"), b"my filter").unwrap();
        fs::write(temporary.path().join("d2gl.json"), b"my display").unwrap();
        fs::write(temporary.path().join("notes.txt"), b"mine").unwrap();
        let mut source = FakeSource::new(&[
            ("Game.exe", b"new game"),
            ("ProjectDiablo.dll", b"new dll"),
            ("loot.filter", b"vendor default"),
            ("d2gl.json", b"vendor display"),
        ]);

        let outcome = VerifiedUpdater::new(temporary.path(), &mut source)
            .apply(
                "txn-one",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();

        assert_eq!(
            outcome,
            ApplyOutcome::Applied {
                downloaded: 2,
                promoted: 2,
                preserved: 2,
            }
        );
        assert_eq!(
            fs::read(temporary.path().join("Game.exe")).unwrap(),
            b"new game"
        );
        assert_eq!(
            fs::read(temporary.path().join("loot.filter")).unwrap(),
            b"my filter"
        );
        assert_eq!(
            fs::read(temporary.path().join("d2gl.json")).unwrap(),
            b"my display"
        );
        assert_eq!(
            fs::read(temporary.path().join("notes.txt")).unwrap(),
            b"mine"
        );
    }

    #[test]
    fn files_the_official_launcher_owns_are_preserved_never_overwritten() {
        let temporary = TempDir::new().unwrap();
        // The official launcher replaced itself and added its own files.
        fs::write(temporary.path().join("PD2Launcher.exe"), b"self-updated").unwrap();
        fs::write(temporary.path().join("SteamPD2.exe"), b"steam shim").unwrap();
        fs::create_dir(temporary.path().join("AppData")).unwrap();
        fs::write(
            temporary.path().join("AppData/launcherSettings.json"),
            b"{}",
        )
        .unwrap();
        let mut source = FakeSource::new(&[
            ("Game.exe", b"new game"),
            ("ProjectDiablo.dll", b"new dll"),
            ("PD2Launcher.exe", b"manifest launcher"),
        ]);

        let outcome = VerifiedUpdater::new(temporary.path(), &mut source)
            .apply(
                "txn-official",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();

        assert_eq!(
            outcome,
            ApplyOutcome::Applied {
                downloaded: 2,
                promoted: 2,
                preserved: 1,
            }
        );
        assert_eq!(
            fs::read(temporary.path().join("PD2Launcher.exe")).unwrap(),
            b"self-updated"
        );
        assert_eq!(
            fs::read(temporary.path().join("SteamPD2.exe")).unwrap(),
            b"steam shim"
        );
        assert!(crate::audit::is_official_launcher_file(
            "AppData/launcherSettings.json.backup"
        ));
        assert!(crate::audit::is_official_launcher_file("TempLauncher.exe"));
        assert!(!crate::audit::is_official_launcher_file("AppDataX/file"));
        assert!(!crate::audit::is_official_launcher_file(
            "AppData/notes.txt"
        ));
        assert!(!crate::audit::is_official_launcher_file("AppData"));
        assert!(!crate::audit::is_official_launcher_file("Game.exe"));
    }

    #[test]
    fn update_keeps_staging_promotion_and_cleanup_on_the_retained_managed_root() {
        struct SwappingSource {
            inner: FakeSource,
            project: PathBuf,
            retained: PathBuf,
            outside: PathBuf,
            swapped: bool,
        }

        impl Pd2Source for SwappingSource {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                self.inner.fetch_manifest()
            }

            fn open_object(
                &mut self,
                request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                let response = self.inner.open_object(request)?;
                if !self.swapped {
                    self.swapped = true;
                    fs::rename(&self.project, &self.retained)
                        .map_err(|_| NetworkError::permanent("managed-root swap fixture failed"))?;
                    std::os::unix::fs::symlink(&self.outside, &self.project)
                        .map_err(|_| NetworkError::permanent("managed-root link fixture failed"))?;
                }
                Ok(response)
            }
        }

        let fixture = TempDir::new().unwrap();
        let project = fixture.path().join("ProjectD2");
        let retained = fixture.path().join("ProjectD2-retained");
        let outside = fixture.path().join("outside");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&outside).unwrap();
        let mut source = SwappingSource {
            inner: FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]),
            project: project.clone(),
            retained: retained.clone(),
            outside: outside.clone(),
            swapped: false,
        };

        VerifiedUpdater::new(&project, &mut source)
            .apply(
                "root-swap",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();

        assert_eq!(fs::read(retained.join("Game.exe")).unwrap(), b"new game");
        assert_eq!(
            fs::read(retained.join("ProjectDiablo.dll")).unwrap(),
            b"new dll"
        );
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn accepted_unchanged_client_is_a_zero_object_zero_promotion_no_op() {
        let temporary = TempDir::new().unwrap();
        let files: &[(&str, &[u8])] = &[("Game.exe", b"game"), ("ProjectDiablo.dll", b"dll")];
        let mut first = FakeSource::new(files);
        VerifiedUpdater::new(temporary.path(), &mut first)
            .apply(
                "txn-initial",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();
        let mut second = FakeSource::new(files);

        let outcome = VerifiedUpdater::new(temporary.path(), &mut second)
            .apply(
                "txn-noop",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();

        assert_eq!(outcome, ApplyOutcome::NoOp);
        assert!(second.requests.is_empty());
    }

    #[test]
    fn range_ignored_restarts_partial_and_missing_length_is_allowed() {
        for behavior in [ObjectBehavior::IgnoreRange, ObjectBehavior::MissingLength] {
            let temporary = TempDir::new().unwrap();
            let files: &[(&str, &[u8])] = &[
                ("Game.exe", b"game bytes"),
                ("ProjectDiablo.dll", b"dll bytes"),
            ];
            let mut source = FakeSource::new(files);
            source.behavior = behavior;
            let manifest = Pd2Manifest::parse(ManifestResponse {
                requested_url: PD2_MANIFEST_URL.to_owned(),
                final_url: PD2_MANIFEST_URL.to_owned(),
                declared_length: Some(source.manifest.len() as u64),
                body: source.manifest.clone(),
            })
            .unwrap();
            let layout = UpdateLayout::new(temporary.path());
            layout.prepare_staging(&manifest).unwrap();
            if matches!(behavior, ObjectBehavior::IgnoreRange) {
                let partial = layout.partial_files(manifest.id()).join("partial-0000");
                fs::write(partial, b"gam").unwrap();
            }

            VerifiedUpdater::new(temporary.path(), &mut source)
                .apply(
                    "txn-resume",
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                )
                .unwrap();
            assert_eq!(
                fs::read(temporary.path().join("Game.exe")).unwrap(),
                b"game bytes"
            );
            if matches!(behavior, ObjectBehavior::IgnoreRange) {
                assert_eq!(source.requests[0].range_start, Some(3));
            }
        }
    }

    #[test]
    fn truncation_resumes_but_retry_exhaustion_and_digest_mismatch_never_promote() {
        let files: &[(&str, &[u8])] = &[
            ("Game.exe", b"game bytes"),
            ("ProjectDiablo.dll", b"dll bytes"),
        ];
        let resumable = TempDir::new().unwrap();
        let mut truncating = FakeSource::new(files);
        truncating.behavior = ObjectBehavior::TruncateOnce;
        VerifiedUpdater::new(resumable.path(), &mut truncating)
            .apply(
                "txn-truncate",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();
        assert!(
            truncating
                .requests
                .iter()
                .any(|request| request.range_start.is_some())
        );

        let exhausted = TempDir::new().unwrap();
        let mut failing = FakeSource::new(files);
        failing.transient_failures = default_retry_policy().max_attempts();
        let error = VerifiedUpdater::new(exhausted.path(), &mut failing)
            .apply(
                "txn-retry",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::RetryExhausted));
        assert!(!exhausted.path().join("Game.exe").exists());

        let mismatch = TempDir::new().unwrap();
        let mut corrupt = FakeSource::new(files);
        corrupt.behavior = ObjectBehavior::WrongDigest;
        let error = VerifiedUpdater::new(mismatch.path(), &mut corrupt)
            .apply(
                "txn-digest",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap_err();
        assert!(
            matches!(error, UpdateError::DigestMismatch),
            "unexpected mismatch result: {error:?}"
        );
        assert!(!mismatch.path().join("Game.exe").exists());
    }

    #[test]
    fn a_custom_client_base_allows_redirects_but_keeps_manifest_md5_checks() {
        let files: &[(&str, &[u8])] = &[("Game.exe", b"game"), ("ProjectDiablo.dll", b"dll")];
        let base = "https://mirror.example/pd2";

        let temporary = TempDir::new().unwrap();
        let mut source = FakeSource::with_custom_base(files, base);
        source.behavior = ObjectBehavior::Redirect;
        let outcome = VerifiedUpdater::new(temporary.path(), &mut source)
            .apply(
                "txn-custom",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();
        assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
        assert_eq!(
            fs::read(temporary.path().join("Game.exe")).unwrap(),
            b"game"
        );
        assert!(
            source
                .requests
                .iter()
                .all(|request| request.url.starts_with(base) && request.accept_redirects)
        );

        let mismatch = TempDir::new().unwrap();
        let mut corrupt = FakeSource::with_custom_base(files, base);
        corrupt.behavior = ObjectBehavior::WrongDigest;
        let error = VerifiedUpdater::new(mismatch.path(), &mut corrupt)
            .apply(
                "txn-custom-digest",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::DigestMismatch), "{error:?}");
        assert!(!mismatch.path().join("Game.exe").exists());
    }

    #[test]
    fn redirects_game_activity_and_indeterminate_inventory_fail_before_mutation() {
        let files: &[(&str, &[u8])] = &[("Game.exe", b"game"), ("ProjectDiablo.dll", b"dll")];
        for (game, inventory, expected) in [
            (
                GameActivity::Running,
                ProtectedInventoryEvidence::Complete,
                UpdateError::GameRunning,
            ),
            (
                GameActivity::Idle,
                ProtectedInventoryEvidence::Indeterminate,
                UpdateError::InventoryIndeterminate,
            ),
        ] {
            let temporary = TempDir::new().unwrap();
            let mut source = FakeSource::new(files);
            let error = VerifiedUpdater::new(temporary.path(), &mut source)
                .apply("txn-guard", game, inventory)
                .unwrap_err();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected)
            );
            assert_eq!(source.manifest_fetches, 0);
            assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
        }

        let temporary = TempDir::new().unwrap();
        let mut source = FakeSource::new(files);
        source.behavior = ObjectBehavior::Redirect;
        let error = VerifiedUpdater::new(temporary.path(), &mut source)
            .apply(
                "txn-origin",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::OriginRejected));
        assert!(!temporary.path().join("Game.exe").exists());
    }

    #[test]
    fn declared_oversized_objects_are_rejected_before_writing() {
        let temporary = TempDir::new().unwrap();
        let partial = temporary.path().join("object.part");
        let mut partial_file = open_partial(&partial).unwrap();
        let error = append_response(
            &mut partial_file,
            &ObjectRequest {
                url: object_url("Game.exe"),
                range_start: None,
                accept_redirects: false,
            },
            ObjectResponse {
                final_url: object_url("Game.exe"),
                status: 200,
                declared_length: Some(MAX_OBJECT_BYTES + 1),
                content_range: None,
                retry_after: None,
                body: Box::new(Cursor::new(Vec::new())),
            },
            0,
            1,
            &OperationControl::default(),
        )
        .unwrap_err();

        assert!(matches!(error, UpdateError::ObjectTooLarge));
        assert_eq!(partial_file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn partial_file_open_rejects_links_and_detects_path_substitution() {
        let temporary = TempDir::new().unwrap();
        let target = temporary.path().join("target");
        fs::write(&target, b"outside").unwrap();

        let symlink_path = temporary.path().join("symlink.part");
        symlink(&target, &symlink_path).unwrap();
        assert!(open_partial(&symlink_path).is_err());

        let hardlink_path = temporary.path().join("hardlink.part");
        fs::hard_link(&target, &hardlink_path).unwrap();
        assert!(matches!(
            open_partial(&hardlink_path),
            Err(UpdateError::UnsafePath)
        ));

        let partial = temporary.path().join("object.part");
        let opened = open_partial(&partial).unwrap();
        fs::rename(&partial, temporary.path().join("original.part")).unwrap();
        fs::write(&partial, b"replacement").unwrap();
        assert!(matches!(
            ensure_path_is_open_file(&partial, &opened),
            Err(UpdateError::UnsafePath)
        ));

        let symlink_partial = temporary.path().join("same-object.part");
        let symlink_opened = open_partial(&symlink_partial).unwrap();
        let moved = temporary.path().join("same-object-moved.part");
        fs::rename(&symlink_partial, &moved).unwrap();
        symlink(&moved, &symlink_partial).unwrap();
        assert!(matches!(
            ensure_path_is_open_file(&symlink_partial, &symlink_opened),
            Err(UpdateError::UnsafePath)
        ));
    }

    #[test]
    fn updater_promotes_only_the_partial_descriptor_that_received_the_response() {
        struct SubstitutingSource {
            bytes: Vec<u8>,
            partial: PathBuf,
            retained: PathBuf,
        }

        impl Pd2Source for SubstitutingSource {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                unreachable!()
            }

            fn open_object(
                &mut self,
                request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                Ok(ObjectResponse {
                    final_url: request.url.clone(),
                    status: 200,
                    declared_length: Some(self.bytes.len() as u64),
                    content_range: None,
                    retry_after: None,
                    body: Box::new(SubstituteUpdaterPartial {
                        body: Cursor::new(self.bytes.clone()),
                        partial: self.partial.clone(),
                        retained: self.retained.clone(),
                        substituted: false,
                    }),
                })
            }
        }

        struct SubstituteUpdaterPartial {
            body: Cursor<Vec<u8>>,
            partial: PathBuf,
            retained: PathBuf,
            substituted: bool,
        }

        impl Read for SubstituteUpdaterPartial {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.substituted {
                    self.substituted = true;
                    fs::rename(&self.partial, &self.retained)?;
                    fs::write(&self.partial, b"attacker")?;
                }
                self.body.read(buffer)
            }
        }

        let temporary = TempDir::new().unwrap();
        let bytes = b"verified game bytes";
        let manifest_response = ManifestResponse {
            requested_url: PD2_MANIFEST_URL.to_owned(),
            final_url: PD2_MANIFEST_URL.to_owned(),
            declared_length: None,
            body: manifest_body(&[
                ("Game.exe", bytes),
                ("ProjectDiablo.dll", b"verified dll bytes"),
            ]),
        };
        let manifest = Pd2Manifest::parse(manifest_response).unwrap();
        let layout = UpdateLayout::new(temporary.path());
        let lease = layout.acquire_exclusive().unwrap();
        layout
            .prepare_staging_with_lease("test-download", &manifest, &lease)
            .unwrap();
        let entry = manifest
            .entries()
            .iter()
            .find(|entry| entry.path().as_str() == "Game.exe")
            .unwrap();
        let partial = layout.partial_files(manifest.id()).join("partial-0000");
        let retained = partial.with_file_name("retained-response.part");
        let mut source = SubstitutingSource {
            bytes: bytes.to_vec(),
            partial: partial.clone(),
            retained: retained.clone(),
        };

        assert!(
            download_entry(
                &mut source,
                DownloadEntry {
                    lease: &lease,
                    manifest: &manifest,
                    entry,
                    overall_completed: 0,
                    overall_total: 1,
                    retry_policy: default_retry_policy(),
                    control: &OperationControl::default(),
                },
            )
            .is_err()
        );
        assert_eq!(fs::read(&partial).unwrap(), b"attacker");
        assert_eq!(fs::read(&retained).unwrap(), bytes);
        assert!(
            fs::metadata(layout.staged_files(manifest.id()).join("object-0000"))
                .is_ok_and(|metadata| metadata.len() == 0)
        );
    }

    #[test]
    fn cancellation_during_object_stream_never_reaches_promotion() {
        let temporary = TempDir::new().unwrap();
        let partial = temporary.path().join("object.part");
        let mut partial_file = open_partial(&partial).unwrap();
        let control = OperationControl::default();
        let body = b"object bytes".to_vec();
        let error = append_response(
            &mut partial_file,
            &ObjectRequest {
                url: object_url("Game.exe"),
                range_start: None,
                accept_redirects: false,
            },
            ObjectResponse {
                final_url: object_url("Game.exe"),
                status: 200,
                declared_length: Some(body.len() as u64),
                content_range: None,
                retry_after: None,
                body: Box::new(CancelAfterRead {
                    body: Cursor::new(body),
                    control: control.clone(),
                    requested: false,
                }),
            },
            0,
            1,
            &control,
        )
        .unwrap_err();

        assert!(matches!(error, UpdateError::Cancelled));
        assert!(partial.exists());
        assert!(!temporary.path().join("Game.exe").exists());
    }

    #[test]
    fn cancellation_during_manifest_stream_maps_to_cancelled_before_staging() {
        struct CancellingManifest;

        impl Pd2Source for CancellingManifest {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                unreachable!()
            }

            fn fetch_manifest_controlled(
                &mut self,
                control: &OperationControl,
            ) -> Result<ManifestResponse, NetworkError> {
                control.request_cancel();
                Err(NetworkError::transient("fixture cancellation"))
            }

            fn open_object(
                &mut self,
                _request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                unreachable!()
            }
        }

        let temporary = TempDir::new().unwrap();
        let control = OperationControl::default();
        let error = VerifiedUpdater::new(temporary.path(), &mut CancellingManifest)
            .apply_controlled(
                "cancel-manifest",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
                &control,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::Cancelled));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }

    #[test]
    fn repeated_body_cancellation_reuses_staging_and_reclaims_known_payload_bytes() {
        struct CancellingObject {
            manifest: Vec<u8>,
            control: OperationControl,
        }

        impl Pd2Source for CancellingObject {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                Ok(ManifestResponse {
                    requested_url: PD2_MANIFEST_URL.to_owned(),
                    final_url: PD2_MANIFEST_URL.to_owned(),
                    declared_length: Some(self.manifest.len() as u64),
                    body: self.manifest.clone(),
                })
            }

            fn open_object(
                &mut self,
                request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                let body = b"object bytes".to_vec();
                Ok(ObjectResponse {
                    final_url: request.url.clone(),
                    status: 200,
                    declared_length: Some(body.len() as u64),
                    content_range: None,
                    retry_after: None,
                    body: Box::new(CancelAfterRead {
                        body: Cursor::new(body),
                        control: self.control.clone(),
                        requested: false,
                    }),
                })
            }
        }

        let temporary = TempDir::new().unwrap();
        let manifest = manifest_body(&[
            ("Game.exe", b"object bytes"),
            ("ProjectDiablo.dll", b"dll bytes"),
        ]);
        let mut stable_entry_count = None;
        for sequence in 0..128 {
            let control = OperationControl::default();
            let mut source = CancellingObject {
                manifest: manifest.clone(),
                control: control.clone(),
            };
            let error = VerifiedUpdater::new(temporary.path(), &mut source)
                .apply_controlled(
                    &format!("cancel-{sequence}"),
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                    &control,
                )
                .unwrap_err();
            assert!(matches!(error, UpdateError::Cancelled), "{error:?}");
            assert_eq!(
                crate::audit::TransactionProbe::probe(
                    &crate::transaction::UpdateTransactionProbe::new(UpdateLayout::new(
                        temporary.path(),
                    )),
                )
                .unwrap(),
                crate::audit::TransactionEvidence::Clear
            );

            let entries = recursive_entries(temporary.path());
            match stable_entry_count {
                None => stable_entry_count = Some(entries),
                Some(expected) => assert_eq!(entries, expected),
            }
            for entry in walk_files(temporary.path()) {
                let name = entry.file_name().unwrap().to_string_lossy();
                if name.ends_with(".part") || name == "Game.exe" || name == "manifest.json" {
                    assert_eq!(fs::metadata(entry).unwrap().len(), 0);
                }
            }
        }
    }

    #[test]
    fn cancellation_after_the_final_body_cleans_staging_before_returning_cancelled() {
        let temporary = TempDir::new().unwrap();
        let mut source = FakeSource::new(&[
            ("Game.exe", b"game bytes"),
            ("ProjectDiablo.dll", b"dll bytes"),
        ]);
        let control = OperationControl::default();
        let cancel = control.clone();
        control.observe(Arc::new(move |progress| {
            if progress.phase() == WorkPhase::ClientDownload
                && progress
                    .overall()
                    .is_some_and(|value| value.completed() == value.total().unwrap_or(0))
            {
                cancel.request_cancel();
            }
        }));
        let error = VerifiedUpdater::new(temporary.path(), &mut source)
            .apply_controlled(
                "cancel-after-final-body",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
                &control,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::Cancelled), "{error:?}");
        assert_eq!(
            crate::audit::TransactionProbe::probe(
                &crate::transaction::UpdateTransactionProbe::new(UpdateLayout::new(
                    temporary.path(),
                )),
            )
            .unwrap(),
            crate::audit::TransactionEvidence::Clear
        );
        assert!(!temporary.path().join("Game.exe").exists());
    }

    #[test]
    fn cancellation_cleanup_substitution_returns_typed_indeterminate() {
        let temporary = TempDir::new().unwrap();
        let mut source = FakeSource::new(&[
            ("Game.exe", b"game bytes"),
            ("ProjectDiablo.dll", b"dll bytes"),
        ]);
        let control = OperationControl::default();
        let cancel = control.clone();
        let manifest_path = temporary
            .path()
            .join(".pd2launcher/staging/active/manifest.json");
        let retained_path = manifest_path.with_file_name("retained-manifest.json");
        let observed_manifest = manifest_path.clone();
        let observed_retained = retained_path.clone();
        let substituted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let once = Arc::clone(&substituted);
        control.observe(Arc::new(move |progress| {
            if progress.phase() == WorkPhase::ClientDownload
                && progress
                    .overall()
                    .is_some_and(|value| value.completed() == value.total().unwrap_or(0))
                && !once.swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                cancel.request_cancel();
                fs::rename(&observed_manifest, &observed_retained).unwrap();
                fs::write(&observed_manifest, b"attacker").unwrap();
            }
        }));
        let error = VerifiedUpdater::new(temporary.path(), &mut source)
            .apply_controlled(
                "cancel-cleanup-substituted",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
                &control,
            )
            .unwrap_err();
        assert!(matches!(error, UpdateError::Indeterminate), "{error:?}");
        assert_eq!(fs::read(manifest_path).unwrap(), b"attacker");
        assert!(retained_path.exists());
    }

    fn recursive_entries(root: &Path) -> usize {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                1 + if path.is_dir() {
                    recursive_entries(&path)
                } else {
                    0
                }
            })
            .sum()
    }

    fn walk_files(root: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(walk_files(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    #[test]
    fn stalled_manifest_transport_observes_mutation_safe_stop() {
        struct StalledManifest {
            entered: Option<SyncSender<()>>,
        }

        impl Pd2Source for StalledManifest {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                unreachable!()
            }

            fn fetch_manifest_controlled(
                &mut self,
                control: &OperationControl,
            ) -> Result<ManifestResponse, NetworkError> {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                while !control.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(NetworkError::transient("fixture transport cancelled"))
            }

            fn open_object(
                &mut self,
                _request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                unreachable!()
            }
        }

        let temporary = TempDir::new().unwrap();
        let root = temporary.path().to_path_buf();
        let control = OperationControl::default();
        let worker_control = control.clone();
        let (entered_sender, entered_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            VerifiedUpdater::new(
                root,
                &mut StalledManifest {
                    entered: Some(entered_sender),
                },
            )
            .apply_controlled(
                "stalled-manifest",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
                &worker_control,
            )
        });

        entered_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("manifest transport entered");
        control.request_cancel();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }

    #[test]
    fn stalled_object_body_observes_mutation_safe_stop() {
        struct StalledBodySource {
            manifest: Vec<u8>,
            entered: Option<SyncSender<()>>,
        }

        impl Pd2Source for StalledBodySource {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                Ok(ManifestResponse {
                    requested_url: PD2_MANIFEST_URL.to_owned(),
                    final_url: PD2_MANIFEST_URL.to_owned(),
                    declared_length: Some(self.manifest.len() as u64),
                    body: self.manifest.clone(),
                })
            }

            fn open_object(
                &mut self,
                _request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                unreachable!()
            }

            fn open_object_controlled(
                &mut self,
                request: &ObjectRequest,
                control: &OperationControl,
            ) -> Result<ObjectResponse, NetworkError> {
                Ok(ObjectResponse {
                    final_url: request.url.clone(),
                    status: 200,
                    declared_length: Some(12),
                    content_range: None,
                    retry_after: None,
                    body: Box::new(StalledRead {
                        entered: self.entered.take(),
                        control: control.clone(),
                    }),
                })
            }
        }

        struct StalledRead {
            entered: Option<SyncSender<()>>,
            control: OperationControl,
        }

        impl Read for StalledRead {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                while !self.control.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "fixture body cancelled",
                ))
            }
        }

        let temporary = TempDir::new().unwrap();
        let root = temporary.path().to_path_buf();
        let object = b"object bytes";
        let control = OperationControl::default();
        let worker_control = control.clone();
        let (entered_sender, entered_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            VerifiedUpdater::new(
                root,
                &mut StalledBodySource {
                    manifest: manifest_body(&[
                        ("Game.exe", object),
                        ("ProjectDiablo.dll", b"dll bytes"),
                    ]),
                    entered: Some(entered_sender),
                },
            )
            .apply_controlled(
                "stalled-body",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
                &worker_control,
            )
        });

        if entered_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_err()
        {
            panic!("object body was not entered: {:?}", worker.join().unwrap());
        }
        control.request_cancel();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
        assert!(!temporary.path().join("Game.exe").exists());
    }

    struct CancelAfterRead {
        body: Cursor<Vec<u8>>,
        control: OperationControl,
        requested: bool,
    }

    impl Read for CancelAfterRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.body.read(buffer)?;
            if read > 0 && !self.requested {
                self.requested = true;
                self.control.request_cancel();
            }
            Ok(read)
        }
    }

    /// Every verified cache object: its bytes and file identity.
    fn cache_objects(cache: &Path) -> BTreeMap<String, (Vec<u8>, u64, i64, i64)> {
        fs::read_dir(cache.join(PREFETCH_OBJECTS))
            .unwrap()
            .flatten()
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".partial"))
            .map(|entry| {
                let metadata = entry.metadata().unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    (
                        fs::read(entry.path()).unwrap(),
                        metadata.ino(),
                        metadata.mtime(),
                        metadata.mtime_nsec(),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn a_local_object_copy_is_exact_and_leaves_its_source_unchanged() {
        let temporary = TempDir::new().unwrap();
        let bytes = (0..300_000_u32)
            .map(|value| value as u8)
            .collect::<Vec<_>>();
        fs::write(temporary.path().join("source"), &bytes).unwrap();
        fs::write(temporary.path().join("destination"), vec![b'x'; 400_000]).unwrap();
        let source = File::open(temporary.path().join("source")).unwrap();
        let mut destination = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temporary.path().join("destination"))
            .unwrap();
        copy_local_object(&source, &mut destination).unwrap();
        assert_eq!(
            fs::read(temporary.path().join("destination")).unwrap(),
            bytes
        );
        assert_eq!(fs::read(temporary.path().join("source")).unwrap(), bytes);
        // The copy is independent: changing it never reaches the source.
        destination.set_len(1).unwrap();
        assert_eq!(fs::read(temporary.path().join("source")).unwrap(), bytes);
    }

    struct FailAtFilesystemMutation {
        target: usize,
        current: std::cell::Cell<usize>,
    }

    impl FaultInjector for FailAtFilesystemMutation {
        fn after(&self, event: crate::transaction::FaultEvent) -> io::Result<()> {
            if event != crate::transaction::FaultEvent::FilesystemMutation {
                return Ok(());
            }
            let current = self.current.get() + 1;
            self.current.set(current);
            if current == self.target {
                Err(io::Error::new(io::ErrorKind::Interrupted, "injected stop"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_stop_during_promotion_from_the_cache_is_repaired_and_the_cache_stays_whole() {
        use crate::audit::TransactionProbe as _;
        let cache_path = TempDir::new().unwrap();
        let cache = Directory::open(cache_path.path()).unwrap();
        let mut source =
            FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]);
        prefetch_client(&mut source, &cache, None, &OperationControl::default()).unwrap();
        let downloaded = cache_objects(cache_path.path());

        // Stop at each of the first promotion mutations in turn.
        for target in 1..=4 {
            let project = TempDir::new().unwrap();
            fs::write(project.path().join("Game.exe"), b"old game").unwrap();
            let layout = UpdateLayout::new(project.path());
            let fail = FailAtFilesystemMutation {
                target,
                current: std::cell::Cell::new(0),
            };
            let mut cached =
                PrefetchedPd2Source::new(&mut source, Some(cache.try_clone().unwrap()));
            let stopped = VerifiedUpdater::new(project.path(), &mut cached)
                .with_faults(&fail)
                .apply(
                    &format!("txn-stop-{target}"),
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                );
            assert!(stopped.is_err(), "stop {target}");
            assert_eq!(
                crate::transaction::UpdateTransactionProbe::new(layout.clone())
                    .probe()
                    .unwrap(),
                crate::audit::TransactionEvidence::Repairable,
                "stop {target}"
            );
            TransactionManager::new(&layout, &NoFaults)
                .repair()
                .unwrap();
            assert_eq!(
                crate::transaction::UpdateTransactionProbe::new(layout.clone())
                    .probe()
                    .unwrap(),
                crate::audit::TransactionEvidence::Clear
            );
            assert_eq!(
                fs::read(project.path().join("Game.exe")).unwrap(),
                b"old game",
                "repair restores the file the stopped promotion replaced"
            );
            assert_eq!(cache_objects(cache_path.path()), downloaded);
            // The audit after a repair prunes the finished journals.
            let cleanup =
                crate::transaction::CleanupLease::acquire(&project.path().join("cleanup.lock"))
                    .unwrap();
            TransactionManager::new(&layout, &NoFaults)
                .prune_terminal(
                    crate::transaction::PruneAuditEvidence::FreshAndClear,
                    &cleanup,
                )
                .unwrap();

            // Applying again completes from the cache alone.
            VerifiedUpdater::new(project.path(), &mut cached)
                .apply(
                    &format!("txn-after-repair-{target}"),
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                )
                .unwrap();
            drop(cached);
            assert_eq!(source.requests.len(), 2, "nothing was fetched again");
            assert_eq!(
                fs::read(project.path().join("Game.exe")).unwrap(),
                b"new game"
            );
            assert_eq!(
                fs::read(project.path().join("ProjectDiablo.dll")).unwrap(),
                b"new dll"
            );
            assert_eq!(cache_objects(cache_path.path()), downloaded);
        }
    }

    #[test]
    fn a_stop_while_copying_from_the_cache_leaves_no_staging_and_the_cache_whole() {
        use crate::audit::TransactionProbe as _;
        struct CancelOnSecondCopy<'a> {
            inner: PrefetchedPd2Source<'a>,
            control: &'a OperationControl,
            copies: usize,
        }
        impl Pd2Source for CancelOnSecondCopy<'_> {
            fn fetch_manifest(&mut self) -> Result<ManifestResponse, NetworkError> {
                self.inner.fetch_manifest()
            }
            fn fetch_manifest_controlled(
                &mut self,
                control: &OperationControl,
            ) -> Result<ManifestResponse, NetworkError> {
                self.inner.fetch_manifest_controlled(control)
            }
            fn open_object(
                &mut self,
                request: &ObjectRequest,
            ) -> Result<ObjectResponse, NetworkError> {
                self.inner.open_object(request)
            }
            fn local_object(&mut self, url: &str) -> Option<File> {
                self.copies += 1;
                if self.copies == 2 {
                    self.control.request_cancel();
                }
                self.inner.local_object(url)
            }
        }

        let cache_path = TempDir::new().unwrap();
        let cache = Directory::open(cache_path.path()).unwrap();
        let mut source =
            FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]);
        prefetch_client(&mut source, &cache, None, &OperationControl::default()).unwrap();
        let downloaded = cache_objects(cache_path.path());
        let project = TempDir::new().unwrap();
        fs::write(project.path().join("Game.exe"), b"old game").unwrap();
        let control = OperationControl::default();
        {
            let mut stopping = CancelOnSecondCopy {
                inner: PrefetchedPd2Source::new(&mut source, Some(cache.try_clone().unwrap())),
                control: &control,
                copies: 0,
            };
            assert!(matches!(
                VerifiedUpdater::new(project.path(), &mut stopping).apply_controlled(
                    "txn-stop-copy",
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                    &control,
                ),
                Err(UpdateError::Cancelled)
            ));
        }
        let layout = UpdateLayout::new(project.path());
        assert_eq!(
            crate::transaction::UpdateTransactionProbe::new(layout)
                .probe()
                .unwrap(),
            crate::audit::TransactionEvidence::Clear,
            "nothing was journaled, and the unjournaled staging is gone"
        );
        assert_eq!(
            fs::read(project.path().join("Game.exe")).unwrap(),
            b"old game"
        );
        assert!(!project.path().join("ProjectDiablo.dll").exists());
        assert_eq!(cache_objects(cache_path.path()), downloaded);
        assert_eq!(source.requests.len(), 2);
    }

    #[test]
    fn an_update_keeps_the_cache_one_whole_client_and_seeds_it_from_the_install() {
        let cache_path = TempDir::new().unwrap();
        let cache = Directory::open(cache_path.path()).unwrap();
        let project = TempDir::new().unwrap();
        let installed = Directory::open(project.path()).unwrap();
        let manifest_identity = || {
            let metadata = fs::metadata(cache_path.path().join(PREFETCH_MANIFEST)).unwrap();
            (metadata.ino(), metadata.mtime_nsec())
        };

        // The client is in ProjectD2 but its cache was emptied.
        // Refreshing copies it, fetching nothing.
        let mut first =
            FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]);
        VerifiedUpdater::new(project.path(), &mut first)
            .apply(
                "txn-installed",
                GameActivity::Idle,
                ProtectedInventoryEvidence::Complete,
            )
            .unwrap();
        assert_eq!(first.requests.len(), 2);
        prefetch_client(
            &mut first,
            &cache,
            Some(&installed),
            &OperationControl::default(),
        )
        .unwrap();
        assert_eq!(first.requests.len(), 2, "seeded from ProjectD2");
        let seeded = cache_objects(cache_path.path());
        let mut expected = vec![md5_hex(b"new dll"), md5_hex(b"new game")];
        expected.sort();
        assert_eq!(seeded.keys().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(seeded[&md5_hex(b"new game")].0, b"new game");
        // An unchanged manifest is not rewritten.
        let manifest_before = manifest_identity();
        prefetch_client(
            &mut first,
            &cache,
            Some(&installed),
            &OperationControl::default(),
        )
        .unwrap();
        assert_eq!(manifest_identity(), manifest_before);
        assert_eq!(cache_objects(cache_path.path()), seeded);

        // An update: one file changed, one added, one unchanged. Only the
        // changed and added objects are fetched, the replaced one is
        // pruned, and applying fetches nothing more.
        let mut update = FakeSource::new(&[
            ("Game.exe", b"newer game"),
            ("ProjectDiablo.dll", b"new dll"),
            ("Added.mpq", b"added"),
        ]);
        prefetch_client(
            &mut update,
            &cache,
            Some(&installed),
            &OperationControl::default(),
        )
        .unwrap();
        let mut fetched = update
            .requests
            .iter()
            .map(|request| request.url.clone())
            .collect::<Vec<_>>();
        fetched.sort();
        assert_eq!(fetched, [object_url("Added.mpq"), object_url("Game.exe")]);
        let refreshed = cache_objects(cache_path.path());
        let mut expected = vec![
            md5_hex(b"added"),
            md5_hex(b"new dll"),
            md5_hex(b"newer game"),
        ];
        expected.sort();
        assert_eq!(refreshed.keys().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(
            refreshed[&md5_hex(b"new dll")],
            seeded[&md5_hex(b"new dll")],
            "an unchanged object is kept as it was"
        );
        assert_ne!(manifest_identity(), manifest_before);
        // The audit between the two applies prunes the finished journal.
        let cleanup =
            crate::transaction::CleanupLease::acquire(&cache_path.path().join("cleanup.lock"))
                .unwrap();
        TransactionManager::new(&UpdateLayout::new(project.path()), &NoFaults)
            .prune_terminal(
                crate::transaction::PruneAuditEvidence::FreshAndClear,
                &cleanup,
            )
            .unwrap();
        {
            let mut cached =
                PrefetchedPd2Source::new(&mut update, Some(cache.try_clone().unwrap()));
            let outcome = VerifiedUpdater::new(project.path(), &mut cached)
                .apply(
                    "txn-update",
                    GameActivity::Idle,
                    ProtectedInventoryEvidence::Complete,
                )
                .unwrap();
            assert!(matches!(outcome, ApplyOutcome::Applied { promoted: 2, .. }));
        }
        assert_eq!(update.requests.len(), 2, "applying fetched nothing more");
        assert_eq!(
            fs::read(project.path().join("Game.exe")).unwrap(),
            b"newer game"
        );
        assert_eq!(
            fs::read(project.path().join("Added.mpq")).unwrap(),
            b"added"
        );
        assert_eq!(cache_objects(cache_path.path()), refreshed);
    }

    #[test]
    fn an_installed_file_that_does_not_match_is_never_copied_into_the_cache() {
        let cache_path = TempDir::new().unwrap();
        let cache = Directory::open(cache_path.path()).unwrap();
        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(project.path().join("Game.exe"), b"changed game").unwrap();
        fs::write(outside.path().join("dll"), b"new dll").unwrap();
        symlink(
            outside.path().join("dll"),
            project.path().join("ProjectDiablo.dll"),
        )
        .unwrap();
        let installed = Directory::open(project.path()).unwrap();
        let mut source =
            FakeSource::new(&[("Game.exe", b"new game"), ("ProjectDiablo.dll", b"new dll")]);
        prefetch_client(
            &mut source,
            &cache,
            Some(&installed),
            &OperationControl::default(),
        )
        .unwrap();
        assert_eq!(
            source.requests.len(),
            2,
            "a changed file and a link are fetched"
        );
        let objects = cache_objects(cache_path.path());
        assert_eq!(objects[&md5_hex(b"new game")].0, b"new game");
        assert_eq!(objects[&md5_hex(b"new dll")].0, b"new dll");
    }

    fn manifest_body(files: &[(&str, &[u8])]) -> Vec<u8> {
        let checksum: Vec<_> = files
            .iter()
            .map(|(name, bytes)| serde_json::Value::String(format!("{}  {name}", md5_hex(bytes))))
            .collect();
        serde_json::to_vec(&serde_json::json!({ "checksum": checksum })).unwrap()
    }

    fn md5_hex(bytes: &[u8]) -> String {
        hex(&Md5::digest(bytes))
    }

    fn object_url(name: &str) -> String {
        format!("{PD2_ORIGIN}/{}", name.replace(' ', "%20"))
    }
}
