//! Launcher-owned installation workflow.
//!
//! Installation orchestration depends on domain policy and narrow acquisition,
//! execution, transaction, and persistence interfaces. It never renders UI.

use std::{
    borrow::Cow,
    error::Error,
    ffi::OsStr,
    fmt::{self, Write as _},
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroize;

pub(crate) use crate::downloads::DownloadPlan;
use crate::{
    InstallationStatus, InterruptedStage,
    audit::{CheckStatus, HostSupport},
    game_folder::{ExistingInstall, ExistingInstallProblem, GameFolder, ImportMode},
    operation::{OperationControl, ProgressUnit, WorkPhase, WorkProgress},
    safe_fs::Directory,
};

mod adapters;

pub(crate) use adapters::{
    InstallerMedia, ProductionArtifactSource, ProductionInstallDriver, UpdaterClientSynchronizer,
    XINPUT_BRIDGE_PATH, XINPUT_BRIDGE_SHA256, XINPUT_BRIDGE_SIZE, android_gl_probe_copy,
    fex_strict_settings, gl_probe_helper, install_xinput_bridge, recorded_cache_names,
    remove_wine_gecko_and_mono, seed_verified_cache,
};

/// A temporary authorization that deliberately cannot be cloned or serialized.
pub(crate) struct InstallationAuthorization {
    base_key: ProductKey,
    expansion_key: ProductKey,
    vendor_terms_accepted: bool,
    /// Lets tests see when the authorization is dropped.
    #[cfg(test)]
    alive: std::sync::Arc<()>,
}

impl InstallationAuthorization {
    /// Dead once this authorization is dropped.
    #[cfg(test)]
    pub(crate) fn liveness(&self) -> std::sync::Weak<()> {
        std::sync::Arc::downgrade(&self.alive)
    }

    pub(crate) fn new(
        base_key: String,
        expansion_key: String,
        vendor_terms_accepted: bool,
    ) -> Result<Self, AuthorizationError> {
        // Consume both submitted strings into wiping containers before any
        // early return. Otherwise a missing-consent or first-key error would
        // drop the other key as an ordinary, non-wiping `String`.
        let base_key = ProductKey::new(base_key);
        let expansion_key = ProductKey::new(expansion_key);
        if !vendor_terms_accepted {
            return Err(AuthorizationError::ConsentRequired);
        }
        let base_key = base_key.map_err(|_| AuthorizationError::InvalidBaseKey)?;
        let expansion_key = expansion_key.map_err(|_| AuthorizationError::InvalidExpansionKey)?;
        Ok(Self {
            base_key,
            expansion_key,
            vendor_terms_accepted,
            #[cfg(test)]
            alive: std::sync::Arc::new(()),
        })
    }

    fn key_for(&self, product: VendorProduct) -> &ProductKey {
        match product {
            VendorProduct::BaseGame => &self.base_key,
            VendorProduct::Expansion => &self.expansion_key,
        }
    }
}

impl fmt::Debug for InstallationAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstallationAuthorization")
            .field("base_key", &Redacted)
            .field("expansion_key", &Redacted)
            .field("vendor_terms_accepted", &self.vendor_terms_accepted)
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

struct ProductKey {
    bytes: Box<[u8]>,
}

impl ProductKey {
    fn new(value: String) -> Result<Self, ()> {
        let mut bytes = value.into_bytes();
        if bytes.is_empty()
            || bytes.len() > 64
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            bytes.zeroize();
            return Err(());
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
        })
    }

    fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for ProductKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// Stable authorization failures that cannot contain submitted key material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorizationError {
    ConsentRequired,
    InvalidBaseKey,
    InvalidExpansionKey,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsentRequired => formatter.write_str("vendor-term consent is required"),
            Self::InvalidBaseKey => formatter.write_str("the D2 product key format is invalid"),
            Self::InvalidExpansionKey => {
                formatter.write_str("the LoD product key format is invalid")
            }
        }
    }
}

impl Error for AuthorizationError {}

/// Exact external inputs used by the Managed installation workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactKind {
    ManagedRuntime,
    Gecko,
    /// Wine Mono, installed silently so Wine never shows its Mono prompt.
    WineMono,
    /// .NET 6 Desktop Runtime (x86), which the official PD2 launcher needs.
    DotnetDesktop,
    BaseGameWrapper,
    ExpansionWrapper,
    ClientBootstrap,
    /// The built-in FEX's package, for this device's CPU level.
    FexPackage,
    /// The built-in FEX's RootFS.
    FexRootFs,
}

impl ArtifactKind {
    pub(crate) const fn download_artifact(self) -> crate::sources::DownloadArtifact {
        use crate::sources::DownloadArtifact;
        match self {
            Self::ManagedRuntime => DownloadArtifact::Wine,
            Self::Gecko => DownloadArtifact::Gecko,
            Self::WineMono => DownloadArtifact::WineMono,
            Self::DotnetDesktop => DownloadArtifact::DotnetDesktop,
            Self::BaseGameWrapper => DownloadArtifact::D2Installer,
            Self::ExpansionWrapper => DownloadArtifact::LodInstaller,
            Self::ClientBootstrap => DownloadArtifact::Pd2Installer,
            Self::FexPackage => DownloadArtifact::Fex,
            Self::FexRootFs => DownloadArtifact::FexRootFs,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RedirectPolicy {
    None,
    GithubReleaseAsset,
    /// A Launchpad PPA's file link (`…/+files/<name>`), which sends the
    /// request on to the Launchpad librarian's copy of the same file name.
    LaunchpadLibrarian,
    /// Custom sources only: any `http`/`https` destination, bounded hops.
    AnyHttp,
}

impl RedirectPolicy {
    /// The policy of a built-in source, from where it is.
    fn for_builtin_source(url: &str) -> Self {
        if url.starts_with("https://github.com/") {
            Self::GithubReleaseAsset
        } else if url.starts_with("https://launchpad.net/") {
            Self::LaunchpadLibrarian
        } else {
            Self::None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SizePolicy {
    Exact(u64),
    Bounded { min: u64, max: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntegrityPolicy {
    Sha256(&'static str),
    Sha512(&'static str),
    PeExecutable,
    /// A user-supplied source: content is deliberately not
    /// verified. Size caps, staging, and extraction rules still apply.
    Unverified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactSpec {
    kind: ArtifactKind,
    request_url: Cow<'static, str>,
    redirects: RedirectPolicy,
    size: SizePolicy,
    integrity: IntegrityPolicy,
    expected_filename: Option<&'static str>,
    /// Other built-in sources of the same bytes, tried in order when
    /// `request_url` does not give them. Each has the size and digest above
    /// and the redirect policy of where it is.
    fallback_urls: &'static [&'static str],
}

impl ArtifactSpec {
    pub(crate) fn request_url(&self) -> &str {
        &self.request_url
    }

    /// Every source of this artifact in the order they are tried, each as a
    /// spec of its own: `request_url` first, then each fallback.
    pub(crate) fn sources(&self) -> impl Iterator<Item = Self> + '_ {
        let only = |request_url: Cow<'static, str>, redirects| Self {
            request_url,
            redirects,
            fallback_urls: &[],
            ..self.clone()
        };
        std::iter::once(only(self.request_url.clone(), self.redirects)).chain(
            self.fallback_urls
                .iter()
                .map(move |url| only(Cow::Borrowed(*url), RedirectPolicy::for_builtin_source(url))),
        )
    }

    /// Whether an artifact requested from `requested_url` and served from
    /// `final_url` came from one of this spec's sources.
    fn approves_source(&self, requested_url: &str, final_url: &str) -> bool {
        self.sources()
            .find(|source| source.request_url() == requested_url)
            .is_some_and(|source| source.approves_final_url(final_url))
    }

    pub(crate) fn kind(&self) -> ArtifactKind {
        self.kind
    }

    /// A user-supplied source: any `http`/`https` URL, redirects allowed, no
    /// digest, size-exactness, PE, or filename verification; only the
    /// generous size cap remains.
    fn custom(kind: ArtifactKind, url: String, maximum_bytes: u64) -> Self {
        Self {
            kind,
            request_url: Cow::Owned(url),
            redirects: RedirectPolicy::AnyHttp,
            size: SizePolicy::Bounded {
                min: 1,
                max: maximum_bytes,
            },
            integrity: IntegrityPolicy::Unverified,
            expected_filename: None,
            fallback_urls: &[],
        }
    }

    pub(crate) fn is_custom(&self) -> bool {
        self.integrity == IntegrityPolicy::Unverified
    }

    pub(crate) fn maximum_bytes(&self) -> u64 {
        match self.size {
            SizePolicy::Exact(size) => size,
            SizePolicy::Bounded { max, .. } => max,
        }
    }

    /// Whether the acquisition adapter may follow or accept this final URL.
    /// Adapters must apply this before streaming a redirected response; the
    /// workflow repeats it after download against retained provenance.
    pub(crate) fn approves_final_url(&self, final_url: &str) -> bool {
        match self.redirects {
            RedirectPolicy::None => final_url == self.request_url,
            RedirectPolicy::AnyHttp => zed_reqwest::Url::parse(final_url).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            }),
            RedirectPolicy::GithubReleaseAsset => {
                final_url == self.request_url
                    || final_url
                        .strip_prefix("https://release-assets.githubusercontent.com/")
                        .is_some_and(|rest| {
                            !rest.is_empty()
                                && !rest.contains('\\')
                                && !rest.contains('#')
                                && !rest.split('/').any(|part| matches!(part, "." | ".."))
                        })
            }
            // `https://launchpadlibrarian.net/<number>/<the requested file name>`.
            RedirectPolicy::LaunchpadLibrarian => {
                final_url == self.request_url
                    || final_url
                        .strip_prefix("https://launchpadlibrarian.net/")
                        .and_then(|rest| rest.split_once('/'))
                        .is_some_and(|(number, name)| {
                            !number.is_empty()
                                && number.bytes().all(|byte| byte.is_ascii_digit())
                                && !name.is_empty()
                                && self
                                    .request_url
                                    .rsplit_once("/+files/")
                                    .is_some_and(|(_, requested)| requested == name)
                        })
            }
        }
    }
}

const WINE_MONO_URL: &str = "https://dl.winehq.org/wine/wine-mono/11.3.0/wine-mono-11.3.0-x86.msi";
const DOTNET_DESKTOP_URL: &str = "https://builds.dotnet.microsoft.com/dotnet/WindowsDesktop/6.0.36/windowsdesktop-runtime-6.0.36-win-x86.exe";

/// Acquisition contracts. The built-in entries are frozen and
/// updating one requires a reviewed Launcher release. A user may replace an
/// entry with a custom, unverified source through
/// [`ArtifactCatalog::with_sources`]; built-in entries are never relaxed.
#[derive(Clone, Debug)]
pub(crate) struct ArtifactCatalog {
    specs: [ArtifactSpec; 9],
}

impl ArtifactCatalog {
    fn new(specs: [ArtifactSpec; 9]) -> Self {
        Self { specs }
    }

    pub(crate) fn pinned() -> Self {
        let runtime = crate::runtime::current();
        let fex = crate::builtin_fex::FexVariant::current();
        Self::new([
            ArtifactSpec {
                kind: ArtifactKind::ManagedRuntime,
                request_url: Cow::Borrowed(runtime.archive_url),
                redirects: RedirectPolicy::GithubReleaseAsset,
                size: SizePolicy::Exact(runtime.archive_size),
                integrity: IntegrityPolicy::Sha512(runtime.archive_sha512),
                expected_filename: None,
                fallback_urls: &[],
            },
            ArtifactSpec {
                kind: ArtifactKind::Gecko,
                request_url: Cow::Borrowed(
                    "https://dl.winehq.org/wine/wine-gecko/2.47.4/wine-gecko-2.47.4-x86.msi",
                ),
                redirects: RedirectPolicy::None,
                size: SizePolicy::Exact(55_187_968),
                integrity: IntegrityPolicy::Sha256(
                    "26cecc47706b091908f7f814bddb074c61beb8063318e9efc5a7f789857793d6",
                ),
                expected_filename: None,
                fallback_urls: &[],
            },
            ArtifactSpec {
                kind: ArtifactKind::WineMono,
                request_url: Cow::Borrowed(WINE_MONO_URL),
                redirects: RedirectPolicy::None,
                size: SizePolicy::Exact(83_348_992),
                integrity: IntegrityPolicy::Sha256(crate::audit::WINE_MONO_MSI_SHA256),
                expected_filename: None,
                fallback_urls: &[],
            },
            ArtifactSpec {
                kind: ArtifactKind::DotnetDesktop,
                request_url: Cow::Borrowed(DOTNET_DESKTOP_URL),
                redirects: RedirectPolicy::None,
                size: SizePolicy::Exact(52_173_224),
                // The SHA-512 Microsoft publishes in its 6.0 releases.json.
                integrity: IntegrityPolicy::Sha512(
                    "a18351aabfe1590e58af79e57ac2414254ba80cb7a1fef19545a6b8418575c735fc1dc164c3c7fed426c4698f099991487fa4f443bab93afd41d1563845fbcf4",
                ),
                expected_filename: None,
                fallback_urls: &[],
            },
            ArtifactSpec {
                kind: ArtifactKind::BaseGameWrapper,
                request_url: Cow::Borrowed(
                    "https://downloader.battle.net/download/getLegacy?os=win&locale=enUS&product=D2DV",
                ),
                redirects: RedirectPolicy::None,
                size: SizePolicy::Bounded {
                    min: 1_000_000,
                    max: 8_000_000,
                },
                integrity: IntegrityPolicy::PeExecutable,
                expected_filename: Some("Downloader_Diablo2_enUS.exe"),
                fallback_urls: &[],
            },
            ArtifactSpec {
                kind: ArtifactKind::ExpansionWrapper,
                request_url: Cow::Borrowed(
                    "https://downloader.battle.net/download/getLegacy?os=win&locale=enUS&product=D2XP",
                ),
                redirects: RedirectPolicy::None,
                size: SizePolicy::Bounded {
                    min: 1_000_000,
                    max: 8_000_000,
                },
                integrity: IntegrityPolicy::PeExecutable,
                expected_filename: Some("Downloader_Diablo2_Lord_of_Destruction_enUS.exe"),
                fallback_urls: &[],
            },
            ArtifactSpec {
                kind: ArtifactKind::ClientBootstrap,
                request_url: Cow::Borrowed(
                    "https://storage.googleapis.com/project-diablo-2-website-public/installer/ProjectD2Installer.exe",
                ),
                redirects: RedirectPolicy::None,
                size: SizePolicy::Exact(37_374_204),
                integrity: IntegrityPolicy::Sha256(
                    "50ef80e41ef626a14a2cd584b915e79774ad0b03d51cd03af719a6fbf8d93677",
                ),
                expected_filename: None,
                fallback_urls: &[],
            },
            Self::builtin_fex_spec(ArtifactKind::FexPackage, fex.package()),
            Self::builtin_fex_spec(ArtifactKind::FexRootFs, crate::builtin_fex::rootfs()),
        ])
    }

    /// A built-in FEX file: from the project's mirror release first, then
    /// from its upstream sources, the same bytes from each.
    fn builtin_fex_spec(
        kind: ArtifactKind,
        file: &'static crate::builtin_fex::PinnedFile,
    ) -> ArtifactSpec {
        ArtifactSpec {
            kind,
            request_url: Cow::Borrowed(file.mirror_url()),
            redirects: RedirectPolicy::for_builtin_source(file.mirror_url()),
            size: SizePolicy::Exact(file.size()),
            integrity: IntegrityPolicy::Sha256(file.sha256()),
            expected_filename: None,
            fallback_urls: file.upstream(),
        }
    }

    /// The built-in catalog with each custom source in `sources` replacing
    /// its built-in entry.
    pub(crate) fn with_sources(sources: &crate::sources::DownloadSources) -> Self {
        let mut catalog = Self::pinned();
        for spec in &mut catalog.specs {
            if let Some(url) = sources.custom_url(spec.kind.download_artifact()) {
                *spec = ArtifactSpec::custom(
                    spec.kind,
                    url.to_owned(),
                    crate::sources::CUSTOM_ARTIFACT_MAX_BYTES,
                );
            }
        }
        catalog
    }

    pub(crate) fn request_url(&self, kind: ArtifactKind) -> &str {
        self.spec(kind).request_url()
    }

    pub(crate) fn spec(&self, kind: ArtifactKind) -> &ArtifactSpec {
        self.specs
            .iter()
            .find(|spec| spec.kind == kind)
            .expect("artifact catalog must contain every workflow input")
    }
}

/// Provenance returned by the approved network adapter after a bounded,
/// partial-file download has closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcquiredArtifact {
    path: PathBuf,
    origin: ArtifactOrigin,
    requested_url: String,
    final_url: String,
    suggested_filename: Option<String>,
    retained_receipt_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ArtifactOrigin {
    ApprovedNetwork,
    UserSelected,
}

impl AcquiredArtifact {
    pub(crate) fn new(
        path: PathBuf,
        requested_url: impl Into<String>,
        final_url: impl Into<String>,
        suggested_filename: Option<String>,
    ) -> Self {
        Self {
            path,
            origin: ArtifactOrigin::ApprovedNetwork,
            requested_url: requested_url.into(),
            final_url: final_url.into(),
            suggested_filename,
            retained_receipt_sha256: None,
        }
    }

    pub(crate) fn user_selected(path: PathBuf, filename: impl Into<String>) -> Self {
        Self {
            path,
            origin: ArtifactOrigin::UserSelected,
            requested_url: String::new(),
            final_url: String::new(),
            suggested_filename: Some(filename.into()),
            retained_receipt_sha256: None,
        }
    }

    fn with_retained_receipt_sha256(mut self, digest: impl Into<String>) -> Self {
        self.retained_receipt_sha256 = Some(digest.into());
        self
    }
}

/// Closed and independently identity-checked artifact safe to pass to a
/// stage-specific adapter. It is still never executable by the verifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedArtifact {
    kind: ArtifactKind,
    path: PathBuf,
    origin: ArtifactOrigin,
    observed_sha256: String,
    retained_receipt_sha256: Option<String>,
    requested_url: String,
    final_url: String,
    /// The user-supplied request URL when this artifact came from a custom,
    /// unverified source; `None` for built-in, verified sources.
    custom_source: Option<String>,
}

impl VerifiedArtifact {
    pub(crate) fn kind(&self) -> ArtifactKind {
        self.kind
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn custom_source(&self) -> Option<&str> {
        self.custom_source.as_deref()
    }

    pub(crate) fn content_sha256(&self) -> &str {
        &self.observed_sha256
    }

    fn execution_sha256(&self) -> Result<&str, ArtifactRejection> {
        if matches!(
            self.kind,
            ArtifactKind::BaseGameWrapper | ArtifactKind::ExpansionWrapper
        ) {
            self.retained_receipt_sha256
                .as_deref()
                .filter(|digest| *digest == self.observed_sha256)
                .ok_or(ArtifactRejection::DigestMismatch)
        } else {
            Ok(&self.observed_sha256)
        }
    }

    #[cfg(test)]
    pub(crate) fn observed_sha256(&self) -> &str {
        &self.observed_sha256
    }

    #[cfg(test)]
    pub(crate) fn requested_url(&self) -> &str {
        &self.requested_url
    }

    #[cfg(test)]
    pub(crate) fn final_url(&self) -> &str {
        &self.final_url
    }

    #[cfg(test)]
    pub(crate) fn origin(&self) -> ArtifactOrigin {
        self.origin
    }
}

/// External acquisition boundary. Implementations must download into a unique
/// partial below `cache` and retain that exact open file through verification
/// and safe promotion before returning its provenance.
pub(crate) trait ArtifactSource {
    fn acquire_at(
        &self,
        spec: &ArtifactSpec,
        cache: &Path,
        cache_directory: &Directory,
        control: &OperationControl,
    ) -> Result<AcquiredArtifact, AdapterFailure>;

    #[cfg(test)]
    fn acquire(
        &self,
        spec: &ArtifactSpec,
        cache: &Path,
        control: &OperationControl,
    ) -> Result<AcquiredArtifact, AdapterFailure> {
        let directory = Directory::open(cache).map_err(|_| AdapterFailure::here())?;
        self.acquire_at(spec, cache, &directory, control)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactRejection {
    UnsafePath,
    UnexpectedType,
    SizeMismatch,
    RequestUrlMismatch,
    UnapprovedFinalUrl,
    UnexpectedFilename,
    DigestMismatch,
    InvalidExecutable,
    Unreadable,
}

fn verify_artifact_at(
    spec: &ArtifactSpec,
    cache: &Path,
    cache_directory: &Directory,
    artifact: AcquiredArtifact,
) -> Result<VerifiedArtifact, ArtifactRejection> {
    validate_artifact_provenance(spec, &artifact)?;
    let name = artifact
        .path
        .file_name()
        .ok_or(ArtifactRejection::UnsafePath)?
        .to_owned();
    let direct_cache_entry = artifact.path == cache.join(&name);
    let complete_payload_entry = match spec.kind {
        ArtifactKind::BaseGameWrapper => {
            artifact.path == cache.join("d2-payload").join("Installer.exe")
        }
        ArtifactKind::ExpansionWrapper => {
            artifact.path == cache.join("lod-payload").join("Installer.exe")
        }
        _ => false,
    };
    if !direct_cache_entry && !complete_payload_entry {
        return Err(ArtifactRejection::UnsafePath);
    }
    let relative = artifact
        .path
        .strip_prefix(cache)
        .map_err(|_| ArtifactRejection::UnsafePath)?;
    let mut file = cache_directory
        .open_file(relative)
        .map_err(|_| ArtifactRejection::Unreadable)?;
    let metadata = file.metadata().map_err(|_| ArtifactRejection::Unreadable)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(ArtifactRejection::UnexpectedType);
    }
    let retained_path = artifact.path.clone();
    verify_open_artifact(spec, artifact, retained_path, &mut file)
}

#[cfg(test)]
fn verify_artifact(
    spec: &ArtifactSpec,
    cache: &Path,
    artifact: AcquiredArtifact,
) -> Result<VerifiedArtifact, ArtifactRejection> {
    let directory = Directory::open(cache).map_err(|_| ArtifactRejection::UnsafePath)?;
    verify_artifact_at(spec, cache, &directory, artifact)
}

fn validate_artifact_provenance(
    spec: &ArtifactSpec,
    artifact: &AcquiredArtifact,
) -> Result<(), ArtifactRejection> {
    match artifact.origin {
        ArtifactOrigin::ApprovedNetwork => {
            if !spec
                .sources()
                .any(|source| source.request_url() == artifact.requested_url)
            {
                return Err(ArtifactRejection::RequestUrlMismatch);
            }
            if !spec.approves_source(&artifact.requested_url, &artifact.final_url) {
                return Err(ArtifactRejection::UnapprovedFinalUrl);
            }
        }
        ArtifactOrigin::UserSelected
            if !matches!(
                spec.kind,
                ArtifactKind::BaseGameWrapper | ArtifactKind::ExpansionWrapper
            ) =>
        {
            return Err(ArtifactRejection::UnapprovedFinalUrl);
        }
        ArtifactOrigin::UserSelected => {}
    }
    if spec
        .expected_filename
        .is_some_and(|expected| artifact.suggested_filename.as_deref() != Some(expected))
    {
        return Err(ArtifactRejection::UnexpectedFilename);
    }
    Ok(())
}

fn verify_open_artifact(
    spec: &ArtifactSpec,
    artifact: AcquiredArtifact,
    retained_path: PathBuf,
    file: &mut File,
) -> Result<VerifiedArtifact, ArtifactRejection> {
    validate_artifact_provenance(spec, &artifact)?;
    let metadata = file.metadata().map_err(|_| ArtifactRejection::Unreadable)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(ArtifactRejection::UnexpectedType);
    }
    match spec.size {
        SizePolicy::Exact(expected) if metadata.len() != expected => {
            return Err(ArtifactRejection::SizeMismatch);
        }
        SizePolicy::Bounded { min, max } if metadata.len() < min || metadata.len() > max => {
            return Err(ArtifactRejection::SizeMismatch);
        }
        SizePolicy::Exact(_) | SizePolicy::Bounded { .. } => {}
    }

    let (sha256, sha512) = hash_open_artifact(file)?;
    match spec.integrity {
        IntegrityPolicy::Sha256(expected) if sha256 != expected => {
            return Err(ArtifactRejection::DigestMismatch);
        }
        IntegrityPolicy::Sha512(expected) if sha512 != expected => {
            return Err(ArtifactRejection::DigestMismatch);
        }
        IntegrityPolicy::PeExecutable => validate_open_pe(file)?,
        IntegrityPolicy::Sha256(_) | IntegrityPolicy::Sha512(_) | IntegrityPolicy::Unverified => {}
    }
    if matches!(spec.integrity, IntegrityPolicy::PeExecutable)
        && artifact.retained_receipt_sha256.as_deref() != Some(sha256.as_str())
    {
        return Err(ArtifactRejection::DigestMismatch);
    }

    Ok(VerifiedArtifact {
        kind: spec.kind,
        path: retained_path,
        origin: artifact.origin,
        observed_sha256: sha256,
        retained_receipt_sha256: artifact.retained_receipt_sha256,
        requested_url: artifact.requested_url,
        final_url: artifact.final_url,
        custom_source: spec.is_custom().then(|| spec.request_url().to_owned()),
    })
}

fn hash_open_artifact(file: &mut File) -> Result<(String, String), ArtifactRejection> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ArtifactRejection::Unreadable)?;
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| ArtifactRejection::Unreadable)?;
        if count == 0 {
            break;
        }
        sha256.update(&buffer[..count]);
        sha512.update(&buffer[..count]);
    }
    Ok((hex(&sha256.finalize()), hex(&sha512.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn validate_open_pe(file: &mut File) -> Result<(), ArtifactRejection> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ArtifactRejection::InvalidExecutable)?;
    let mut dos = [0_u8; 64];
    file.read_exact(&mut dos)
        .map_err(|_| ArtifactRejection::InvalidExecutable)?;
    if &dos[..2] != b"MZ" {
        return Err(ArtifactRejection::InvalidExecutable);
    }
    let offset = u32::from_le_bytes(
        dos[0x3c..0x40]
            .try_into()
            .expect("DOS header slice is four bytes"),
    );
    file.seek(SeekFrom::Start(u64::from(offset)))
        .map_err(|_| ArtifactRejection::InvalidExecutable)?;
    let mut signature = [0_u8; 4];
    file.read_exact(&mut signature)
        .map_err(|_| ArtifactRejection::InvalidExecutable)?;
    if signature != *b"PE\0\0" {
        return Err(ArtifactRejection::InvalidExecutable);
    }
    Ok(())
}

/// Read-only projection of the full Auditor report used for stage planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InstallAudit {
    status: InstallationStatus,
    host: HostSupport,
    runtime: CheckStatus,
    prefix: CheckStatus,
    prefix_needs: PrefixNeeds,
    /// The player chose the built-in FEX and it is not installed.
    builtin_fex_needed: bool,
    /// The Android host: Blizzard's
    /// downloaders run in Install, not Download. The app provides the
    /// RootFS, the Wine runtime and the Wine components.
    android_host: bool,
}

impl InstallAudit {
    /// Without component evidence, an unaccepted prefix needs every
    /// prefix component.
    pub(crate) fn new(
        status: InstallationStatus,
        host: HostSupport,
        runtime: CheckStatus,
        prefix: CheckStatus,
    ) -> Self {
        let needed = prefix != CheckStatus::Satisfied;
        Self {
            status,
            host,
            runtime,
            prefix,
            prefix_needs: PrefixNeeds {
                gecko: needed,
                mono: needed,
                dotnet: needed,
            },
            builtin_fex_needed: false,
            android_host: false,
        }
    }

    /// The Android host. Its prefix never gets Wine Gecko or Wine Mono: no
    /// program the Launcher runs there needs them, so neither is downloaded
    /// or installed.
    pub(crate) fn with_android_host(mut self) -> Self {
        self.android_host = true;
        self.prefix_needs.gecko = false;
        self.prefix_needs.mono = false;
        self
    }

    #[cfg(test)]
    pub(crate) fn prefix_needs(&self) -> PrefixNeeds {
        self.prefix_needs
    }

    #[cfg(test)]
    pub(crate) fn android_host(&self) -> bool {
        self.android_host
    }

    /// The built-in FEX must be installed before Wine runs.
    pub(crate) fn with_builtin_fex_needed(mut self, needed: bool) -> Self {
        self.builtin_fex_needed = needed;
        self
    }

    /// Whether the Launcher's Wine archive is needed: its runtime is not
    /// accepted. Never on Android, whose app provides the runtime (its
    /// preflight needs the prefix, so before Install it is not accepted).
    fn needs_runtime_archive(&self) -> bool {
        !self.android_host && self.runtime != CheckStatus::Satisfied
    }

    /// Only the prefix components the audit rejected are provisioned.
    pub(crate) fn with_prefix_needs(mut self, needs: PrefixNeeds) -> Self {
        if self.prefix != CheckStatus::Satisfied {
            self.prefix_needs = needs;
        }
        self
    }
}

/// Prefix components the audit did not accept. An existing prefix is kept
/// and only these are installed into it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PrefixNeeds {
    pub(crate) gecko: bool,
    pub(crate) mono: bool,
    pub(crate) dotnet: bool,
}

/// Verified prefix-component installers for one provisioning pass; `None`
/// keeps an accepted component exactly as audited.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PrefixArtifacts<'a> {
    pub(crate) gecko: Option<&'a VerifiedArtifact>,
    pub(crate) mono: Option<&'a VerifiedArtifact>,
    pub(crate) dotnet: Option<&'a VerifiedArtifact>,
}

pub(crate) trait InstallationAuditor {
    fn audit(&self, paths: &ManagedPaths) -> Result<InstallAudit, AdapterFailure>;
}

/// Stable Launcher-owned locations. Adapters receive these paths rather than
/// choosing an installation or prefix destination themselves.
pub(crate) struct ManagedPaths {
    directory: Directory,
    root: PathBuf,
    cache: PathBuf,
    runtime: PathBuf,
    prefix: PathBuf,
    game: PathBuf,
    game_folder: GameFolder,
}

impl fmt::Debug for ManagedPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedPaths")
            .field("root", &self.root)
            .field("cache", &self.cache)
            .field("runtime", &self.runtime)
            .field("prefix", &self.prefix)
            .field("game", &self.game)
            .finish_non_exhaustive()
    }
}

impl ManagedPaths {
    pub(crate) fn prepare(root: &Path) -> Result<Self, InstallError> {
        if !root.is_absolute() {
            return Err(InstallError::ManagedRootInvalid);
        }
        let directory =
            Directory::ensure_absolute(root).map_err(|_| InstallError::ManagedRootInvalid)?;
        let game_folder =
            GameFolder::read(&directory).map_err(|_| InstallError::GameFolderUnavailable)?;
        Ok(Self {
            game: game_folder.host_path(root),
            directory,
            root: root.to_owned(),
            cache: root.join("cache/artifacts"),
            runtime: root.join("runner").join(crate::runtime::current().identity),
            prefix: root.join("prefix"),
            game_folder,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn directory(&self) -> &Directory {
        &self.directory
    }

    #[cfg(test)]
    pub(crate) fn runtime(&self) -> &Path {
        &self.runtime
    }

    #[cfg(test)]
    pub(crate) fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// The game folder's host path: the Managed one, or a folder used in
    /// place.
    pub(crate) fn game(&self) -> &Path {
        &self.game
    }

    /// Open the game folder: beneath the root, or a folder used in place
    /// proven to be the recorded one.
    pub(crate) fn open_game(&self) -> std::io::Result<Directory> {
        self.game_folder.open(&self.directory)
    }

    /// The recorded game folder: the Managed one or a folder used in place.
    pub(crate) fn game_folder(&self) -> &GameFolder {
        &self.game_folder
    }

    /// The game folder as Windows programs in the Managed prefix see it.
    pub(crate) fn windows_game(&self) -> String {
        self.game_folder.windows_path()
    }
}

/// Installer-side effect boundary. Implementations must use typed process
/// arguments, wait for the Launcher-owned process family and wineserver to be
/// quiescent, and write receipts only after the documented durable evidence is
/// present. Returning success never advances the workflow by itself.
pub(crate) trait InstallDriver {
    /// Provision the components the audit rejected: `wine` when the runtime
    /// was not accepted, and each prefix component (Gecko, Wine Mono, .NET
    /// Desktop Runtime) the prefix audit rejected. `None` keeps the accepted
    /// component exactly as audited.
    fn provision_runtime(
        &self,
        paths: &ManagedPaths,
        wine: Option<&VerifiedArtifact>,
        prefix: PrefixArtifacts<'_>,
        mutation_lease: &File,
        control: &OperationControl,
    ) -> Result<(), AdapterFailure>;

    /// Run a Blizzard downloader wrapper to fetch its product's complete
    /// installer, verify it against its frozen identity, and keep it as that
    /// product's complete installer in the artifact cache. The wrapper never
    /// receives a product key and nothing it fetched runs.
    fn download_vendor_payload(
        &self,
        paths: &ManagedPaths,
        wrapper: &VerifiedArtifact,
        mutation_lease: &File,
        control: &OperationControl,
    ) -> Result<(), VendorFailure>;

    fn install_vendor(
        &self,
        request: VendorInstallRequest<'_>,
        mutation_lease: &File,
        control: &OperationControl,
    ) -> Result<(), VendorFailure>;

    /// Make a validated Diablo II installation the game: copy it into
    /// the Managed game folder, or record it for use in place; then write
    /// the registry the game reads and, last, the import receipt.
    fn import_existing(
        &self,
        paths: &ManagedPaths,
        existing: &ExistingInstall,
        mode: ImportMode,
        mutation_lease: &File,
        control: &OperationControl,
    ) -> Result<(), AdapterFailure>;

    fn install_client_bootstrap(
        &self,
        paths: &ManagedPaths,
        bootstrap: &VerifiedArtifact,
        mutation_lease: &File,
        control: &OperationControl,
    ) -> Result<(), AdapterFailure>;
}

/// Verified client synchronization is supplied by the updater workflow. This
/// keeps installation ordering here while manifest/staging policy remains in
/// `update`.
pub(crate) trait ClientSynchronizer {
    /// Download and verify every client file of the current manifest into
    /// the Launcher-owned cache (`cache/client`) without installing any.
    fn prefetch(
        &self,
        paths: &ManagedPaths,
        control: &OperationControl,
    ) -> Result<(), AdapterFailure>;

    fn synchronize(
        &self,
        paths: &ManagedPaths,
        control: &OperationControl,
    ) -> Result<(), AdapterFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VendorProduct {
    BaseGame,
    Expansion,
}

impl VendorProduct {
    const fn artifact_kind(self) -> ArtifactKind {
        match self {
            Self::BaseGame => ArtifactKind::BaseGameWrapper,
            Self::Expansion => ArtifactKind::ExpansionWrapper,
        }
    }

    const fn download_stage(self) -> InstallStage {
        match self {
            Self::BaseGame => InstallStage::BaseGameDownload,
            Self::Expansion => InstallStage::ExpansionDownload,
        }
    }
}

pub(crate) struct VendorInstallRequest<'a> {
    product: VendorProduct,
    artifact: &'a VerifiedArtifact,
    paths: &'a ManagedPaths,
    product_key: &'a ProductKey,
}

impl VendorInstallRequest<'_> {
    pub(crate) fn product(&self) -> VendorProduct {
        self.product
    }

    pub(crate) fn artifact(&self) -> &VerifiedArtifact {
        self.artifact
    }

    pub(crate) fn paths(&self) -> &ManagedPaths {
        self.paths
    }

    /// Exposes the key only for the duration of one adapter-owned callback so
    /// workflow APIs cannot return or format it accidentally.
    pub(crate) fn with_product_key<T>(&self, use_key: impl FnOnce(&[u8]) -> T) -> T {
        use_key(self.product_key.expose())
    }
}

impl fmt::Debug for VendorInstallRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VendorInstallRequest")
            .field("product", &self.product)
            .field("artifact", &self.artifact.path)
            .field("paths", &self.paths)
            .field("product_key", &Redacted)
            .finish()
    }
}

/// Boundary failures intentionally carry no adapter-provided text, because a
/// Vendor installer may echo private inputs in its own error rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdapterFailure;

impl AdapterFailure {
    /// A failure at the caller's source line, which goes to standard error
    /// (some systems, such as DroidDeck, keep nothing else). Only the line
    /// is printed, never data. Some of these failures are expected and
    /// handled by the caller.
    #[track_caller]
    pub(crate) fn here() -> Self {
        let at = std::panic::Location::caller();
        eprintln!(
            "pd2-launcher: install step stopped at {}:{} (it may be handled)",
            at.file(),
            at.line()
        );
        Self
    }
}

/// Why the download cache could not be used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheProblem {
    /// An entry the cache does not recognise, or an unsafe one.
    Inventory(ArtifactRejection),
    /// The cache folder itself could not be opened safely.
    Folder,
    /// A stopped write's temporary files could not be cleaned up.
    RetainedState,
}

/// Where a Vendor stage failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VendorFailure {
    /// Blizzard's downloader ended without a verified payload in some other
    /// way (no window, could not be stopped, its own exit, its timeout), so
    /// no installer ran. A Blizzard wrapper's embedded download authorization
    /// expires about a week after it was fetched, so this and the next three
    /// are retried once with a freshly downloaded wrapper.
    WrapperDownload,
    /// Blizzard's downloader showed a message box (its helper prints the
    /// text).
    DownloaderErrorDialog,
    /// The downloader ended with its download unfinished: a file missing or
    /// still a `.part`.
    PayloadIncomplete,
    /// The downloader finished, but a file differs from its frozen identity
    /// (size, hash, an unexpected entry).
    PayloadRejected,
    /// Blizzard's downloader got no new data for its stall limit, as when
    /// the connection is lost: a network failure, so the wrapper is kept and
    /// the stage fails at once.
    DownloadStalled,
    /// The installer showed its error dialog about the product key it was
    /// given: the key was rejected.
    KeyRejected,
    /// The installer showed any other error dialog.
    ErrorDialog,
    /// The installer stayed at its main menu instead of installing.
    InstallerMenu,
    /// Anything else, including a failure after the installer started.
    Install,
}

impl VendorFailure {
    /// Whether a freshly downloaded wrapper may succeed where this failed:
    /// every downloader failure but a stall, which is the connection's.
    fn may_need_fresh_wrapper(self) -> bool {
        matches!(
            self,
            Self::WrapperDownload
                | Self::DownloaderErrorDialog
                | Self::PayloadIncomplete
                | Self::PayloadRejected
        )
    }

    /// How Blizzard's downloader failed, when this is its failure.
    fn downloader_failure(self) -> Option<DownloaderFailure> {
        match self {
            Self::WrapperDownload => Some(DownloaderFailure::Ended),
            Self::DownloaderErrorDialog => Some(DownloaderFailure::ErrorDialog),
            Self::PayloadIncomplete => Some(DownloaderFailure::Incomplete),
            Self::PayloadRejected => Some(DownloaderFailure::Rejected),
            Self::DownloadStalled => Some(DownloaderFailure::Stalled),
            Self::KeyRejected | Self::ErrorDialog | Self::InstallerMenu | Self::Install => None,
        }
    }
}

/// How Blizzard's downloader failed to deliver a verified payload, each with
/// its own failure code (`install_failure_code`): a lost connection, an
/// error it reported, a download it left unfinished, files that differ from
/// their identity, or any other end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DownloaderFailure {
    Stalled,
    ErrorDialog,
    Incomplete,
    Rejected,
    Ended,
}

impl From<AdapterFailure> for VendorFailure {
    fn from(_: AdapterFailure) -> Self {
        Self::Install
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallStage {
    /// Recording the Download result.
    Download,
    ManagedRuntime,
    /// Blizzard's downloader fetching the Diablo II installer.
    BaseGameDownload,
    /// Blizzard's downloader fetching the Lord of Destruction installer.
    ExpansionDownload,
    /// Verifying the Project Diablo 2 client files into the cache.
    ClientPrefetch,
    BaseGame,
    Expansion,
    /// Copying or recording a Diablo II the user already has.
    Import,
    ClientBootstrap,
    ClientSynchronization,
    /// Unpacking the built-in FEX and its RootFS.
    BuiltinFex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstallOutcome {
    final_status: InstallationStatus,
    completed_stages: Vec<InstallStage>,
}

impl InstallOutcome {
    pub(crate) fn final_status(&self) -> InstallationStatus {
        self.final_status
    }

    #[cfg(test)]
    pub(crate) fn completed_stages(&self) -> &[InstallStage] {
        &self.completed_stages
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InstallError {
    ManagedRootInvalid,
    /// The recorded game folder used in place is unavailable.
    GameFolderUnavailable,
    InstallationBusy,
    AuditUnavailable,
    UnsupportedHost,
    HostEvidenceUnavailable,
    AuthorizationRequired,
    RepairRequired,
    Indeterminate,
    /// The audit found an interrupted stage; only Start over continues.
    StageInterrupted,
    RuntimeEvidenceInvalid,
    AcquisitionFailed {
        artifact: ArtifactKind,
    },
    /// The download cache itself cannot be used, before anything is
    /// downloaded: entries the Launcher does not recognise, or retained
    /// write state it cannot account for (a folder copied mid-write, or
    /// from another computer). Never a network failure.
    DownloadCacheUnusable {
        artifact: ArtifactKind,
        cause: CacheProblem,
    },
    ArtifactRejected {
        artifact: ArtifactKind,
        reason: ArtifactRejection,
    },
    StageFailed {
        stage: InstallStage,
    },
    /// Blizzard's downloader for `stage` (a download stage) delivered no
    /// verified payload, as `failure` says.
    DownloaderFailed {
        stage: InstallStage,
        failure: DownloaderFailure,
    },
    /// The Vendor installer showed its error dialog about the product key
    /// for `stage`: that key was rejected.
    KeyRejected {
        stage: InstallStage,
    },
    /// The Vendor installer for `stage` showed another error dialog.
    InstallerError {
        stage: InstallStage,
    },
    /// The Vendor installer for `stage` stayed at its main menu instead of
    /// installing.
    InstallerMenu {
        stage: InstallStage,
    },
    EvidenceDidNotAdvance {
        stage: InstallStage,
        expected: InstallationStatus,
        observed: InstallationStatus,
    },
    /// Diablo II is already installed; this step no longer applies.
    BaseGameInstalled,
    /// Project Diablo 2 needs Diablo II installed first.
    BaseGameRequired,
    /// The existing installation is no longer a usable Diablo II folder.
    ExistingInstallInvalid(ExistingInstallProblem),
    Cancelled,
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "managed installation stopped: {self:?}")
    }
}

impl Error for InstallError {}

pub(crate) struct ManagedInstaller<'a> {
    auditor: &'a dyn InstallationAuditor,
    artifacts: &'a dyn ArtifactSource,
    driver: &'a dyn InstallDriver,
    client: &'a dyn ClientSynchronizer,
    catalog: ArtifactCatalog,
    /// The Download job runs: acquisitions are downloads. In the install
    /// jobs they only reverify the cache ahead of installing.
    downloading: std::cell::Cell<bool>,
}

/// What the Download job verified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DownloadOutcome {
    /// Bytes of every verified download the rest of the install uses.
    pub(crate) total_bytes: u64,
}

impl<'a> ManagedInstaller<'a> {
    pub(crate) fn new(
        auditor: &'a dyn InstallationAuditor,
        artifacts: &'a dyn ArtifactSource,
        driver: &'a dyn InstallDriver,
        client: &'a dyn ClientSynchronizer,
        catalog: ArtifactCatalog,
    ) -> Self {
        Self {
            auditor,
            artifacts,
            driver,
            client,
            catalog,
            downloading: std::cell::Cell::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) fn run(
        &mut self,
        managed_root: &Path,
        authorization: Option<&InstallationAuthorization>,
    ) -> Result<InstallOutcome, InstallError> {
        self.run_controlled(managed_root, authorization, &OperationControl::default())
    }

    /// Every step in order, as unattended mode runs them: Download, then
    /// Install Diablo II, then Install Project Diablo 2.
    #[cfg(test)]
    pub(crate) fn run_controlled(
        &mut self,
        managed_root: &Path,
        authorization: Option<&InstallationAuthorization>,
        control: &OperationControl,
    ) -> Result<InstallOutcome, InstallError> {
        self.download_all(managed_root, DownloadPlan::Installers, control)?;
        let mut outcome = self.install_base_game(managed_root, authorization, control)?;
        if outcome.final_status == InstallationStatus::Ready {
            return Ok(outcome);
        }
        let project = self.install_project_diablo(managed_root, control)?;
        outcome.completed_stages.extend(project.completed_stages);
        outcome.final_status = project.final_status;
        Ok(outcome)
    }

    /// Step 3, Download: acquire and verify everything the remaining steps
    /// need, so they run from verified local files. Blizzard's installers
    /// are fetched by their downloader into the Launcher-owned cache (which
    /// needs the Wine runtime and prefix first); the Project Diablo 2 client
    /// files are verified into the cache but not installed. The result is
    /// recorded for "downloads complete". No CD key is needed.
    pub(crate) fn download_all(
        &mut self,
        managed_root: &Path,
        plan: DownloadPlan,
        control: &OperationControl,
    ) -> Result<DownloadOutcome, InstallError> {
        self.downloading.set(true);
        let result = self.download_everything(managed_root, plan, control);
        self.downloading.set(false);
        result
    }

    /// Only the built-in FEX the player chose, for an
    /// installation whose other downloads are done: from Settings, or after
    /// choosing it once the game was installed.
    pub(crate) fn install_builtin_fex_only(
        &mut self,
        managed_root: &Path,
        control: &OperationControl,
    ) -> Result<DownloadOutcome, InstallError> {
        self.downloading.set(true);
        let result = (|| {
            let paths = ManagedPaths::prepare(managed_root)?;
            drop(MutationLease::acquire(&paths)?);
            check_cancelled(control)?;
            let report = self.audit(&paths)?;
            check_host(report.host)?;
            check_recovery_status(report.status)?;
            let total_bytes = if report.builtin_fex_needed {
                self.install_builtin_fex(&paths, control)?
            } else {
                0
            };
            Ok(DownloadOutcome { total_bytes })
        })();
        self.downloading.set(false);
        result
    }

    fn download_everything(
        &self,
        managed_root: &Path,
        plan: DownloadPlan,
        control: &OperationControl,
    ) -> Result<DownloadOutcome, InstallError> {
        let paths = ManagedPaths::prepare(managed_root)?;
        drop(MutationLease::acquire(&paths)?);
        check_cancelled(control)?;
        let mut report = self.audit(&paths)?;
        check_host(report.host)?;
        check_recovery_status(report.status)?;
        // The built-in FEX comes first: every Wine program after it, Blizzard's
        // downloader included, runs through it.
        let mut fex_bytes = 0;
        if report.builtin_fex_needed {
            fex_bytes = self.install_builtin_fex(&paths, control)?;
        }
        if report.status == InstallationStatus::Ready {
            return Ok(DownloadOutcome {
                total_bytes: fex_bytes,
            });
        }
        {
            let _lease = MutationLease::acquire(&paths)?;
            crate::downloads::clear(paths.directory()).map_err(|_| InstallError::StageFailed {
                stage: InstallStage::Download,
            })?;
        }
        let status = report.status;
        let installers = plan == DownloadPlan::Installers;
        let vendor: &[VendorProduct] = match status {
            InstallationStatus::NeedsBaseGame if installers => {
                &[VendorProduct::BaseGame, VendorProduct::Expansion]
            }
            InstallationStatus::NeedsExpansion if installers => &[VendorProduct::Expansion],
            _ => &[],
        };
        let client = status != InstallationStatus::NeedsRuntime;
        let needs = report.prefix_needs;
        let prefix_needed = report.prefix != CheckStatus::Satisfied;
        // What these downloads reach, so a later need beyond it (after Start
        // over, say) is not taken as served.
        let need = crate::downloads::DownloadNeed {
            prefix: prefix_needed,
            ..crate::downloads::DownloadNeed::for_status(status, prefix_needed)
        };
        let mut kinds = Vec::new();
        if report.needs_runtime_archive() {
            kinds.push(ArtifactKind::ManagedRuntime);
        }
        for (needed, kind) in [
            (needs.gecko, ArtifactKind::Gecko),
            (needs.mono, ArtifactKind::WineMono),
            (needs.dotnet, ArtifactKind::DotnetDesktop),
        ] {
            if prefix_needed && needed {
                kinds.push(kind);
            }
        }
        for product in vendor {
            kinds.push(product.artifact_kind());
        }
        if client {
            kinds.push(ArtifactKind::ClientBootstrap);
        }
        let steps = kinds.len() as u64 + u64::from(client);
        let mut recorded = Vec::new();
        let mut step = 0_u64;
        for kind in kinds {
            control.set_step(step, steps);
            step += 1;
            // On Android Blizzard's downloaders run in Install; Download
            // keeps their wrappers, verified.
            if matches!(
                kind,
                ArtifactKind::BaseGameWrapper | ArtifactKind::ExpansionWrapper
            ) && !report.android_host
            {
                let product = if kind == ArtifactKind::BaseGameWrapper {
                    VendorProduct::BaseGame
                } else {
                    VendorProduct::Expansion
                };
                report = self.download_vendor_payload(&paths, report, product, control)?;
                recorded.push(payload_record_path(kind));
                continue;
            }
            let artifact = {
                let _lease = MutationLease::acquire(&paths)?;
                self.acquire(&paths, kind, control)?
            };
            recorded.push(if is_complete_payload(&paths, &artifact) {
                payload_record_path(kind)
            } else {
                root_relative(&paths, artifact.path())?
            });
        }
        if client {
            control.set_step(step, steps);
            control.report(WorkProgress::for_phase(WorkPhase::ClientDownload));
            let _lease = MutationLease::acquire(&paths)?;
            self.client
                .prefetch(&paths, control)
                .map_err(|_| stage_failure(control, InstallStage::ClientPrefetch))?;
            recorded.push(CLIENT_PREFETCH.to_owned());
        }
        check_cancelled(control)?;
        control.set_step(steps, steps);
        let _lease = MutationLease::acquire(&paths)?;
        let total_bytes = crate::downloads::record(paths.directory(), plan, need, &recorded)
            .map_err(|_| InstallError::StageFailed {
                stage: InstallStage::Download,
            })?;
        Ok(DownloadOutcome { total_bytes })
    }

    /// Download the built-in FEX's package and RootFS, verified, and install
    /// them in the managed root. Their size, in bytes.
    fn install_builtin_fex(
        &self,
        paths: &ManagedPaths,
        control: &OperationControl,
    ) -> Result<u64, InstallError> {
        check_cancelled(control)?;
        let (package, rootfs) = {
            let _lease = MutationLease::acquire(paths)?;
            (
                self.acquire(paths, ArtifactKind::FexPackage, control)?,
                self.acquire(paths, ArtifactKind::FexRootFs, control)?,
            )
        };
        check_cancelled(control)?;
        let _lease = MutationLease::acquire(paths)?;
        crate::builtin_fex::install(
            paths.root(),
            crate::builtin_fex::FexVariant::current(),
            package.path(),
            rootfs.path(),
        )
        .map_err(|error| {
            eprintln!("pd2-launcher: the built-in FEX could not be installed: {error}");
            stage_failure(control, InstallStage::BuiltinFex)
        })?;
        Ok([package.path(), rootfs.path()]
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum())
    }

    /// Fetch one product's complete installer into the cache. A payload
    /// already there (a folder's complete installers, or an earlier
    /// download) is used as it is; otherwise Blizzard's downloader runs in
    /// the Managed prefix, which is set up first when it isn't yet.
    fn download_vendor_payload(
        &self,
        paths: &ManagedPaths,
        mut report: InstallAudit,
        product: VendorProduct,
        control: &OperationControl,
    ) -> Result<InstallAudit, InstallError> {
        let kind = product.artifact_kind();
        let stage = product.download_stage();
        check_cancelled(control)?;
        let mut artifact = {
            let _lease = MutationLease::acquire(paths)?;
            self.acquire(paths, kind, control)?
        };
        if is_complete_payload(paths, &artifact) {
            return Ok(report);
        }
        if report.runtime != CheckStatus::Satisfied || report.prefix != CheckStatus::Satisfied {
            report = self.ensure_runtime(paths, report, control)?;
        }
        control.report(WorkProgress::for_phase(match product {
            VendorProduct::BaseGame => WorkPhase::BaseGameDownload,
            VendorProduct::Expansion => WorkPhase::ExpansionDownload,
        }));
        let lease = MutationLease::acquire(paths)?;
        let mut refetched = false;
        loop {
            match self
                .driver
                .download_vendor_payload(paths, &artifact, &lease.0, control)
            {
                Ok(()) => break,
                // A cached Blizzard wrapper carries a download authorization
                // that expires; discard it and download a fresh one once.
                // Selected media and custom sources report the failure, and
                // so does a stalled download (DownloadStalled).
                Err(failure)
                    if failure.may_need_fresh_wrapper()
                        && !refetched
                        && !control.is_cancelled()
                        && artifact.origin == ArtifactOrigin::ApprovedNetwork
                        && artifact.custom_source().is_none()
                        && !self.catalog.spec(kind).is_custom() =>
                {
                    refetched = true;
                    self.discard_cached_wrapper(paths, kind)?;
                    artifact = self.acquire(paths, kind, control)?;
                    check_cancelled(control)?;
                }
                Err(failure) => {
                    return Err(
                        match (stage_failure(control, stage), failure.downloader_failure()) {
                            (InstallError::StageFailed { stage }, Some(failure)) => {
                                InstallError::DownloaderFailed { stage, failure }
                            }
                            (error, _) => error,
                        },
                    );
                }
            }
        }
        check_cancelled(control)?;
        Ok(report)
    }

    /// Step 4, Install Diablo II: Blizzard's installers for Diablo II and
    /// Lord of Destruction with the CD keys, from the verified downloads.
    /// The game is never patched: Project Diablo 2 brings its own game
    /// files. The Wine runtime and prefix are set up first
    /// when they aren't yet. Ends at `NeedsClient`.
    pub(crate) fn install_base_game(
        &mut self,
        managed_root: &Path,
        authorization: Option<&InstallationAuthorization>,
        control: &OperationControl,
    ) -> Result<InstallOutcome, InstallError> {
        let paths = ManagedPaths::prepare(managed_root)?;
        drop(MutationLease::acquire(&paths)?);
        let mut completed_stages = Vec::new();
        check_cancelled(control)?;
        let mut report = self.audit(&paths)?;
        check_host(report.host)?;
        match report.status {
            // Diablo II is already installed: nothing is left for this step
            // (a missing Wine component is Install Project Diablo 2's).
            InstallationStatus::Ready
            | InstallationStatus::NeedsClient
            | InstallationStatus::NeedsRuntime => {
                return Ok(InstallOutcome {
                    final_status: report.status,
                    completed_stages,
                });
            }
            _ => check_recovery_status(report.status)?,
        }
        if report.runtime != CheckStatus::Satisfied || report.prefix != CheckStatus::Satisfied {
            report = self.ensure_runtime(&paths, report, control)?;
            completed_stages.push(InstallStage::ManagedRuntime);
        }

        // On Android each of Blizzard's downloaders runs here, right before
        // its installer, not in Download: Diablo II is downloaded and
        // installed, then Lord of Destruction.
        if report.status == InstallationStatus::NeedsBaseGame {
            let authorization = authorization.ok_or(InstallError::AuthorizationRequired)?;
            if report.android_host {
                self.download_vendor_payload(&paths, report, VendorProduct::BaseGame, control)?;
            }
            self.install_vendor(&paths, authorization, VendorProduct::BaseGame, control)?;
            completed_stages.push(InstallStage::BaseGame);
            report = self.expect_after(
                &paths,
                InstallStage::BaseGame,
                InstallationStatus::NeedsExpansion,
            )?;
            clear_stage(&paths, InstallStage::BaseGame)?;
        }

        if report.status == InstallationStatus::NeedsExpansion {
            let authorization = authorization.ok_or(InstallError::AuthorizationRequired)?;
            if report.android_host {
                self.download_vendor_payload(&paths, report, VendorProduct::Expansion, control)?;
            }
            self.install_vendor(&paths, authorization, VendorProduct::Expansion, control)?;
            completed_stages.push(InstallStage::Expansion);
            report = self.expect_after(
                &paths,
                InstallStage::Expansion,
                InstallationStatus::NeedsClient,
            )?;
            clear_stage(&paths, InstallStage::Expansion)?;
        }

        Ok(InstallOutcome {
            final_status: report.status,
            completed_stages,
        })
    }

    /// Step 4 for a Diablo II 1.14b or 1.14d the user already has: set up
    /// the Wine runtime and prefix when needed, then copy the game into the
    /// Managed game folder or record it for use in place, write the
    /// registry the game expects, and record the import. No CD
    /// key is needed and the folder is never patched. Ends at `NeedsClient`.
    pub(crate) fn import_existing(
        &mut self,
        managed_root: &Path,
        existing: &ExistingInstall,
        mode: ImportMode,
        control: &OperationControl,
    ) -> Result<InstallOutcome, InstallError> {
        let paths = ManagedPaths::prepare(managed_root)?;
        drop(MutationLease::acquire(&paths)?);
        let mut completed_stages = Vec::new();
        check_cancelled(control)?;
        let mut report = self.audit(&paths)?;
        check_host(report.host)?;
        check_recovery_status(report.status)?;
        // Only a game folder that holds nothing yet is imported into.
        if report.status != InstallationStatus::NeedsBaseGame {
            return Err(InstallError::BaseGameInstalled);
        }
        let existing = existing
            .revalidate(managed_root)
            .map_err(InstallError::ExistingInstallInvalid)?;
        if mode == ImportMode::InPlace && !crate::game_folder::usable_in_place(&existing) {
            return Err(InstallError::ExistingInstallInvalid(
                crate::game_folder::ExistingInstallProblem::UnsupportedPath,
            ));
        }
        if report.runtime != CheckStatus::Satisfied || report.prefix != CheckStatus::Satisfied {
            report = self.ensure_runtime(&paths, report, control)?;
            completed_stages.push(InstallStage::ManagedRuntime);
        }
        debug_assert_eq!(report.status, InstallationStatus::NeedsBaseGame);
        check_cancelled(control)?;
        control.report(match mode {
            ImportMode::Copy => WorkProgress::for_phase(WorkPhase::ImportGame)
                .with_file_bytes(0, Some(existing.copy_bytes())),
            // Nothing is copied: the registry and the record are written.
            ImportMode::InPlace => WorkProgress::for_phase(WorkPhase::ImportGame),
        });
        {
            let lease = MutationLease::acquire(&paths)?;
            // A copy that stops part way leaves files only Start over
            // removes, like a Vendor installer.
            mark_stage(&paths, InterruptedStage::BaseGame, InstallStage::Import)?;
            self.driver
                .import_existing(&paths, &existing, mode, &lease.0, control)
                .map_err(|_| stage_failure(control, InstallStage::Import))?;
        }
        check_cancelled(control)?;
        completed_stages.push(InstallStage::Import);
        let report = self.audit(&paths)?;
        check_host(report.host)?;
        check_recovery_status(report.status)?;
        // A folder used in place may bring the Project Diablo 2 this
        // Launcher installed elsewhere (its ProjectD2 with the update
        // namespace), and so may a copy on the desktop, which brings
        // ProjectD2 along: the audit accepts it, so the PD2 installer is
        // skipped. The Android app's copies are its own; the engine's copy
        // there leaves ProjectD2 out.
        let accepted = report.status == InstallationStatus::NeedsClient
            || (report.status == InstallationStatus::Ready
                && (mode == ImportMode::InPlace || !report.android_host));
        if !accepted {
            return Err(InstallError::EvidenceDidNotAdvance {
                stage: InstallStage::Import,
                expected: InstallationStatus::NeedsClient,
                observed: report.status,
            });
        }
        clear_stage(&paths, InstallStage::Import)?;
        Ok(InstallOutcome {
            final_status: report.status,
            completed_stages,
        })
    }

    /// Step 5, Install Project Diablo 2: the PD2 installer, then the client
    /// files, from the verified downloads (only files that changed since
    /// are fetched). A complete game missing only a Wine component gets
    /// that component first. A ProjectD2 already in the game folder (one
    /// the official PD2 launcher or the PD2 installer put there, brought by
    /// an imported Diablo II) is adopted instead: brought up to date like an
    /// update, with no PD2 installer. Ends at `Ready`.
    pub(crate) fn install_project_diablo(
        &mut self,
        managed_root: &Path,
        control: &OperationControl,
    ) -> Result<InstallOutcome, InstallError> {
        let paths = ManagedPaths::prepare(managed_root)?;
        drop(MutationLease::acquire(&paths)?);
        let mut completed_stages = Vec::new();
        check_cancelled(control)?;
        let mut report = self.audit(&paths)?;
        check_host(report.host)?;
        check_recovery_status(report.status)?;
        match report.status {
            InstallationStatus::Ready => {
                return Ok(InstallOutcome {
                    final_status: InstallationStatus::Ready,
                    completed_stages,
                });
            }
            InstallationStatus::NeedsClient | InstallationStatus::NeedsRuntime => {}
            _ => return Err(InstallError::BaseGameRequired),
        }
        if report.runtime != CheckStatus::Satisfied || report.prefix != CheckStatus::Satisfied {
            report = self.ensure_runtime(&paths, report, control)?;
            completed_stages.push(InstallStage::ManagedRuntime);
        }
        if report.status == InstallationStatus::NeedsClient
            && has_project_diablo(&paths)
            && let Some(adopted) = self.adopt_client(&paths, control)?
        {
            completed_stages.push(InstallStage::ClientSynchronization);
            return Ok(InstallOutcome {
                final_status: adopted.status,
                completed_stages,
            });
        }
        if report.status == InstallationStatus::NeedsClient {
            self.install_client(&paths, control)?;
            completed_stages.push(InstallStage::ClientBootstrap);
            completed_stages.push(InstallStage::ClientSynchronization);
            report = self.expect_after(
                &paths,
                InstallStage::ClientSynchronization,
                InstallationStatus::Ready,
            )?;
        }
        Ok(InstallOutcome {
            final_status: report.status,
            completed_stages,
        })
    }

    /// Provision the Wine runtime and prefix components the audit rejected,
    /// and prove with a fresh audit that nothing else changed.
    fn ensure_runtime(
        &self,
        paths: &ManagedPaths,
        report: InstallAudit,
        control: &OperationControl,
    ) -> Result<InstallAudit, InstallError> {
        // A runtime or Gecko installed from a custom source while the
        // built-in source is configured may be replaced before any later
        // stage. The existing prefix is never recreated: the
        // driver only reinstalls Gecko in place.
        let status = report.status;
        if !matches!(
            status,
            InstallationStatus::NeedsBaseGame
                | InstallationStatus::NeedsExpansion
                | InstallationStatus::NeedsClient
                | InstallationStatus::NeedsRuntime
        ) {
            return Err(InstallError::RuntimeEvidenceInvalid);
        }
        self.provision_runtime(paths, &report, control)?;
        let report = self.audit(paths)?;
        check_host(report.host)?;
        check_recovery_status(report.status)?;
        // Only a complete game missing runtime components becomes Ready
        // here; every other checkpoint is unchanged by provisioning.
        let expected = if status == InstallationStatus::NeedsRuntime {
            InstallationStatus::Ready
        } else {
            status
        };
        if report.status != expected
            || report.runtime != CheckStatus::Satisfied
            || report.prefix != CheckStatus::Satisfied
        {
            return Err(InstallError::RuntimeEvidenceInvalid);
        }
        clear_stage(paths, InstallStage::ManagedRuntime)?;
        Ok(report)
    }

    fn audit(&self, paths: &ManagedPaths) -> Result<InstallAudit, InstallError> {
        self.auditor
            .audit(paths)
            .map_err(|_| InstallError::AuditUnavailable)
    }

    fn expect_after(
        &self,
        paths: &ManagedPaths,
        stage: InstallStage,
        expected: InstallationStatus,
    ) -> Result<InstallAudit, InstallError> {
        let report = self.audit(paths)?;
        check_host(report.host)?;
        check_recovery_status(report.status)?;
        if report.status != expected {
            return Err(InstallError::EvidenceDidNotAdvance {
                stage,
                expected,
                observed: report.status,
            });
        }
        Ok(report)
    }

    fn provision_runtime(
        &self,
        paths: &ManagedPaths,
        report: &InstallAudit,
        control: &OperationControl,
    ) -> Result<(), InstallError> {
        check_cancelled(control)?;
        control.report(WorkProgress::for_phase(if self.downloading.get() {
            WorkPhase::RuntimeAcquisition
        } else {
            WorkPhase::PrefixInitialization
        }));
        if report.builtin_fex_needed {
            self.install_builtin_fex(paths, control)?;
        }
        let lease = MutationLease::acquire(paths)?;
        // Never download an archive whose component the audit accepted.
        let wine = report
            .needs_runtime_archive()
            .then(|| self.acquire(paths, ArtifactKind::ManagedRuntime, control))
            .transpose()?;
        let needs = report.prefix_needs;
        let prefix_needed = report.prefix != CheckStatus::Satisfied;
        let gecko = (prefix_needed && needs.gecko)
            .then(|| self.acquire(paths, ArtifactKind::Gecko, control))
            .transpose()?;
        let mono = (prefix_needed && needs.mono)
            .then(|| self.acquire(paths, ArtifactKind::WineMono, control))
            .transpose()?;
        let dotnet = (prefix_needed && needs.dotnet)
            .then(|| self.acquire(paths, ArtifactKind::DotnetDesktop, control))
            .transpose()?;
        control.report(WorkProgress::for_phase(WorkPhase::PrefixInitialization));
        // Creating a new prefix cannot be resumed from its own evidence: an
        // interruption must be recognizable as such, never as Indeterminate.
        let new_prefix = prefix_needed
            && matches!(
                paths.directory().open_directory(Path::new("prefix")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            );
        if new_prefix {
            mark_stage(
                paths,
                InterruptedStage::Prefix,
                InstallStage::ManagedRuntime,
            )?;
        }
        let prefix = PrefixArtifacts {
            gecko: gecko.as_ref(),
            mono: mono.as_ref(),
            dotnet: dotnet.as_ref(),
        };
        self.driver
            .provision_runtime(paths, wine.as_ref(), prefix, &lease.0, control)
            .map_err(|_| stage_failure(control, InstallStage::ManagedRuntime))?;
        check_cancelled(control)
    }

    fn install_vendor(
        &self,
        paths: &ManagedPaths,
        authorization: &InstallationAuthorization,
        product: VendorProduct,
        control: &OperationControl,
    ) -> Result<(), InstallError> {
        let stage = match product {
            VendorProduct::BaseGame => InstallStage::BaseGame,
            VendorProduct::Expansion => InstallStage::Expansion,
        };
        let artifact_kind = match product {
            VendorProduct::BaseGame => ArtifactKind::BaseGameWrapper,
            VendorProduct::Expansion => ArtifactKind::ExpansionWrapper,
        };
        check_cancelled(control)?;
        control.report(WorkProgress::for_phase(match product {
            VendorProduct::BaseGame => WorkPhase::BaseGameInstall,
            VendorProduct::Expansion => WorkPhase::ExpansionInstall,
        }));
        let lease = MutationLease::acquire(paths)?;
        let artifact = self.acquire(paths, artifact_kind, control)?;
        check_cancelled(control)?;
        mark_stage(
            paths,
            match product {
                VendorProduct::BaseGame => InterruptedStage::BaseGame,
                VendorProduct::Expansion => InterruptedStage::Expansion,
            },
            stage,
        )?;
        self.driver
            .install_vendor(
                VendorInstallRequest {
                    product,
                    artifact: &artifact,
                    paths,
                    product_key: authorization.key_for(product),
                },
                &lease.0,
                control,
            )
            .map_err(|failure| match (stage_failure(control, stage), failure) {
                (InstallError::StageFailed { stage }, VendorFailure::KeyRejected) => {
                    InstallError::KeyRejected { stage }
                }
                (InstallError::StageFailed { stage }, VendorFailure::ErrorDialog) => {
                    InstallError::InstallerError { stage }
                }
                (InstallError::StageFailed { stage }, VendorFailure::InstallerMenu) => {
                    InstallError::InstallerMenu { stage }
                }
                // Its download, not its installer, failed.
                (InstallError::StageFailed { stage }, failure) => {
                    match failure.downloader_failure() {
                        Some(failure) => InstallError::DownloaderFailed {
                            stage: product.download_stage(),
                            failure,
                        },
                        None => InstallError::StageFailed { stage },
                    }
                }
                (error, _) => error,
            })?;
        check_cancelled(control)
    }

    fn discard_cached_wrapper(
        &self,
        paths: &ManagedPaths,
        kind: ArtifactKind,
    ) -> Result<(), InstallError> {
        let rejected = |reason| InstallError::ArtifactRejected {
            artifact: kind,
            reason,
        };
        let cache_directory = paths
            .directory()
            .open_directory(Path::new("cache/artifacts"))
            .map_err(|_| rejected(ArtifactRejection::UnsafePath))?;
        adapters::discard_cached_wrapper(self.catalog.spec(kind), &cache_directory)
            .map_err(|_| rejected(ArtifactRejection::Unreadable))
    }

    fn install_client(
        &self,
        paths: &ManagedPaths,
        control: &OperationControl,
    ) -> Result<(), InstallError> {
        check_cancelled(control)?;
        control.report(WorkProgress::for_phase(WorkPhase::ClientManifest));
        let lease = MutationLease::acquire(paths)?;
        let bootstrap = self.acquire(paths, ArtifactKind::ClientBootstrap, control)?;
        self.driver
            .install_client_bootstrap(paths, &bootstrap, &lease.0, control)
            .map_err(|_| stage_failure(control, InstallStage::ClientBootstrap))?;
        self.client
            .synchronize(paths, control)
            .map_err(|_| stage_failure(control, InstallStage::ClientSynchronization))?;
        check_cancelled(control)
    }

    /// Bring a ProjectD2 the game folder already holds up to date without
    /// the PD2 installer: the verified updater checks every file against
    /// the current manifest, fetches only the ones that differ or are
    /// missing, and writes the Launcher's update records, so later updates
    /// work as usual. `None` when that did not reach `Ready`: the caller
    /// then runs the PD2 installer, as for a game without Project Diablo 2.
    fn adopt_client(
        &self,
        paths: &ManagedPaths,
        control: &OperationControl,
    ) -> Result<Option<InstallAudit>, InstallError> {
        check_cancelled(control)?;
        eprintln!(
            "pd2-launcher: Project Diablo 2 found in the game folder: updating it in place, the PD2 installer is not run"
        );
        control.report(WorkProgress::for_phase(WorkPhase::ClientManifest));
        let synchronized = {
            let _lease = MutationLease::acquire(paths)?;
            self.client.synchronize(paths, control)
        };
        check_cancelled(control)?;
        if synchronized.is_ok() {
            let report = self.audit(paths)?;
            check_host(report.host)?;
            check_recovery_status(report.status)?;
            if report.status == InstallationStatus::Ready {
                return Ok(Some(report));
            }
        }
        eprintln!(
            "pd2-launcher: the Project Diablo 2 found in the game folder could not be updated in place: running the PD2 installer"
        );
        Ok(None)
    }

    fn acquire(
        &self,
        paths: &ManagedPaths,
        kind: ArtifactKind,
        control: &OperationControl,
    ) -> Result<VerifiedArtifact, InstallError> {
        check_cancelled(control)?;
        let downloading = self.downloading.get();
        let (phase, completed_stage) = match kind {
            ArtifactKind::ManagedRuntime
            | ArtifactKind::Gecko
            | ArtifactKind::WineMono
            | ArtifactKind::DotnetDesktop
                if downloading =>
            {
                (WorkPhase::RuntimeAcquisition, 0)
            }
            ArtifactKind::ManagedRuntime
            | ArtifactKind::Gecko
            | ArtifactKind::WineMono
            | ArtifactKind::DotnetDesktop => (WorkPhase::PrefixInitialization, 0),
            ArtifactKind::BaseGameWrapper if downloading => (WorkPhase::BaseGameDownload, 1),
            ArtifactKind::BaseGameWrapper => (WorkPhase::BaseGameInstall, 1),
            ArtifactKind::ExpansionWrapper if downloading => (WorkPhase::ExpansionDownload, 2),
            ArtifactKind::ExpansionWrapper => (WorkPhase::ExpansionInstall, 2),
            ArtifactKind::ClientBootstrap if downloading => (WorkPhase::ClientDownload, 3),
            ArtifactKind::ClientBootstrap => (WorkPhase::ClientManifest, 3),
            ArtifactKind::FexPackage | ArtifactKind::FexRootFs if downloading => {
                (WorkPhase::RuntimeAcquisition, 0)
            }
            ArtifactKind::FexPackage | ArtifactKind::FexRootFs => {
                (WorkPhase::PrefixInitialization, 0)
            }
        };
        control.report(
            WorkProgress::for_phase(phase)
                .with_overall(ProgressUnit::Stages, completed_stage, Some(4))
                .with_verification(0, Some(1)),
        );
        let unusable = |cause| InstallError::DownloadCacheUnusable {
            artifact: kind,
            cause,
        };
        prepare_artifact_cache(paths)
            .map_err(|reason| unusable(CacheProblem::Inventory(reason)))?;
        let spec = self.catalog.spec(kind);
        let cache_directory = paths
            .directory()
            .open_directory(Path::new("cache/artifacts"))
            .map_err(|_| unusable(CacheProblem::Folder))?;
        // An unfinished write of the cache must be cleaned up before a
        // download starts, so a cache that can't be is never reported as a
        // network failure. It removes the temporary files a stopped write
        // left.
        cache_directory
            .recover_atomic()
            .map_err(|_| unusable(CacheProblem::RetainedState))?;
        let acquired = self
            .artifacts
            .acquire_at(spec, &paths.cache, &cache_directory, control)
            .map_err(|_| {
                if control.is_cancelled() {
                    cancellation_error(control)
                } else {
                    InstallError::AcquisitionFailed { artifact: kind }
                }
            })?;
        validate_artifact_cache(paths).map_err(|reason| InstallError::ArtifactRejected {
            artifact: kind,
            reason,
        })?;
        verify_artifact_at(spec, &paths.cache, &cache_directory, acquired).map_err(|reason| {
            InstallError::ArtifactRejected {
                artifact: kind,
                reason,
            }
        })
    }
}

/// Whether the game folder holds a Project Diablo 2 to adopt: a ProjectD2
/// with the client's launch files (`Game.exe`, `ProjectDiablo.dll`) present
/// and not empty. Anything less is installed by the PD2 installer.
fn has_project_diablo(paths: &ManagedPaths) -> bool {
    let Ok(game) = paths.open_game() else {
        return false;
    };
    ["ProjectD2/Game.exe", "ProjectD2/ProjectDiablo.dll"]
        .iter()
        .all(|file| {
            game.open_file(Path::new(file))
                .and_then(|file| file.metadata())
                .is_ok_and(|metadata| metadata.len() > 0)
        })
}

/// Record the stage about to change the prefix. The caller holds the lease.
fn mark_stage(
    paths: &ManagedPaths,
    stage: InterruptedStage,
    install_stage: InstallStage,
) -> Result<(), InstallError> {
    crate::stage_marker::write(paths.directory(), stage).map_err(|_| InstallError::StageFailed {
        stage: install_stage,
    })
}

/// Clear the marker once a fresh audit proved the stage's evidence.
fn clear_stage(paths: &ManagedPaths, install_stage: InstallStage) -> Result<(), InstallError> {
    let _lease = MutationLease::acquire(paths)?;
    crate::stage_marker::clear(paths.directory()).map_err(|_| InstallError::StageFailed {
        stage: install_stage,
    })
}

/// Where the Download job keeps the verified Project Diablo 2 client files.
pub(crate) const CLIENT_PREFETCH: &str = "cache/client";

/// The cache folder that holds a product's complete installer.
fn payload_record_path(kind: ArtifactKind) -> String {
    match kind {
        ArtifactKind::ExpansionWrapper => "cache/artifacts/lod-payload".to_owned(),
        _ => "cache/artifacts/d2-payload".to_owned(),
    }
}

/// Whether `artifact` is a product's complete installer in the cache rather
/// than a downloader wrapper.
fn is_complete_payload(paths: &ManagedPaths, artifact: &VerifiedArtifact) -> bool {
    let directory = match artifact.kind() {
        ArtifactKind::BaseGameWrapper => "d2-payload",
        ArtifactKind::ExpansionWrapper => "lod-payload",
        _ => return false,
    };
    artifact.path() == paths.cache.join(directory).join("Installer.exe")
}

fn root_relative(paths: &ManagedPaths, path: &Path) -> Result<String, InstallError> {
    path.strip_prefix(&paths.root)
        .ok()
        .and_then(Path::to_str)
        .map(str::to_owned)
        .ok_or(InstallError::StageFailed {
            stage: InstallStage::Download,
        })
}

fn check_cancelled(control: &OperationControl) -> Result<(), InstallError> {
    if control.is_cancelled() {
        Err(cancellation_error(control))
    } else {
        Ok(())
    }
}

fn cancellation_error(control: &OperationControl) -> InstallError {
    control.mark_cancelled_safe();
    if control.cancellation_safety() == crate::operation::CancellationSafety::ProvenSafe {
        InstallError::Cancelled
    } else {
        InstallError::Indeterminate
    }
}

fn stage_failure(control: &OperationControl, stage: InstallStage) -> InstallError {
    if control.cancellation_safety() == crate::operation::CancellationSafety::ProvenSafe {
        InstallError::Cancelled
    } else if control.is_cancelled() {
        InstallError::Indeterminate
    } else {
        InstallError::StageFailed { stage }
    }
}

/// Start over: remove the Managed prefix and the stage marker
/// so the next install runs from Diablo II again. `runner/` (the Wine
/// runtime) and `cache/` (verified downloads, including unfinished ones)
/// are kept, and Wine Gecko, Mono, and .NET are reinstalled into the new
/// prefix from that cache. `lease` must be this root's installation lock,
/// held exclusively by the caller for the whole reset.
pub(crate) fn reset_managed_prefix(root: &Directory, lease: &File) -> std::io::Result<()> {
    let lock = root.open_file(Path::new(".pd2launcher.lock"))?;
    if !crate::safe_fs::FileIdentity::of(&lock)?.matches(lease)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "reset requires this root's installation lease",
        ));
    }
    match root.open_directory(Path::new("prefix")) {
        Ok(_) => root.remove_owned_tree(OsStr::new("prefix"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // The row's earlier runtime serves only a prefix it made; with that
    // prefix gone the next install uses the current runtime.
    if let Some(retained) = crate::runtime::retained() {
        match root.open_directory(&Path::new("runner").join(retained.identity)) {
            Ok(_) => root
                .open_directory(Path::new("runner"))?
                .remove_owned_tree(OsStr::new(retained.identity))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    crate::stage_marker::clear(root)?;
    root.sync()
}

fn prepare_artifact_cache(paths: &ManagedPaths) -> Result<(), ArtifactRejection> {
    let cache = paths
        .directory()
        .ensure_directory(Path::new("cache/artifacts"))
        .map_err(|_| ArtifactRejection::UnsafePath)?;
    validate_artifact_inventory(&cache)
}

fn validate_artifact_cache(paths: &ManagedPaths) -> Result<(), ArtifactRejection> {
    let cache = paths
        .directory()
        .open_directory(Path::new("cache"))
        .and_then(|cache| cache.open_directory(Path::new("artifacts")))
        .map_err(|_| ArtifactRejection::UnsafePath)?;
    validate_artifact_inventory(&cache)
}

fn validate_artifact_inventory(cache: &Directory) -> Result<(), ArtifactRejection> {
    const ALLOWED: &[&str] = &[
        crate::runtime::LEGACY_CACHE_NAME,
        crate::runtime::WINE_WOW64_11_11.cache_name,
        // Downloaded for the row's earlier runtime: never used again,
        // tolerated so the caches it filled keep working.
        crate::runtime::WINE_WOW64_11_18.cache_name,
        "wine-gecko-2.47.4-x86.verified",
        "wine-mono-11.3.0-x86.verified",
        "windowsdesktop-runtime-6.0.36-win-x86.verified",
        "d2-wrapper.verified",
        "lod-wrapper.verified",
        "pd2-bootstrap.verified",
        "d2-wrapper.verified.provenance.json",
        "lod-wrapper.verified.provenance.json",
        "custom-wine.download",
        "custom-gecko.download",
        "custom-mono.download",
        "custom-dotnet-desktop.download",
        "custom-d2-installer.download",
        "custom-lod-installer.download",
        "custom-pd2-installer.download",
        "custom-wine.download.provenance.json",
        "custom-gecko.download.provenance.json",
        "custom-mono.download.provenance.json",
        "custom-dotnet-desktop.download.provenance.json",
        "custom-d2-installer.download.provenance.json",
        "custom-lod-installer.download.provenance.json",
        "custom-pd2-installer.download.provenance.json",
        "artifact-00.partial",
        "artifact-01.partial",
        "artifact-02.partial",
        "artifact-03.partial",
        "artifact-04.partial",
        "artifact-05.partial",
        "artifact-06.partial",
        "artifact-07.partial",
        "artifact-08.partial",
        "artifact-09.partial",
        "artifact-00.partial.source",
        "artifact-01.partial.source",
        "artifact-02.partial.source",
        "artifact-03.partial.source",
        "artifact-04.partial.source",
        "artifact-05.partial.source",
        "artifact-06.partial.source",
        "artifact-07.partial.source",
        "artifact-08.partial.source",
        "artifact-09.partial.source",
        // The built-in FEX: each CPU level's package, so a
        // folder moved to another device keeps working.
        "fex-emu-armv8.0_2609.1.verified",
        "fex-emu-armv8.2_2609.1.verified",
        "fex-emu-armv8.4_2609.1.verified",
        crate::builtin_fex::ROOTFS_CACHE_NAME,
        "custom-fex.download",
        "custom-fex-rootfs.download",
        "custom-fex.download.provenance.json",
        "custom-fex-rootfs.download.provenance.json",
        // Left by the retired 1.14d patch stage in caches older releases
        // filled: never used, tolerated so those installations keep
        // working. `artifact-04.partial` above was its download slot.
        "lod-patch-1.14d.verified",
        "custom-patch.download",
        "custom-patch.download.provenance.json",
        "prepatch.log",
    ];
    for name in cache.names().map_err(|_| ArtifactRejection::UnsafePath)? {
        if matches!(name.to_str(), Some("d2-payload" | "lod-payload")) {
            let payload = cache
                .open_directory(Path::new(&name))
                .map_err(|_| ArtifactRejection::UnsafePath)?;
            for payload_name in payload.names().map_err(|_| ArtifactRejection::UnsafePath)? {
                if payload_name == OsStr::new("DirectX") {
                    let directx = payload
                        .open_directory(Path::new(&payload_name))
                        .map_err(|_| ArtifactRejection::UnsafePath)?;
                    for directx_name in
                        directx.names().map_err(|_| ArtifactRejection::UnsafePath)?
                    {
                        if !matches!(
                            directx_name.to_str(),
                            Some(
                                "BDA.cab"
                                    | "BDANT.cab"
                                    | "BDAXP.cab"
                                    | "DSETUP.dll"
                                    | "DirectX.cab"
                                    | "ManagedDX.CAB"
                                    | "dsetup32.dll"
                                    | "dxnt.cab"
                                    | "dxsetup.exe"
                            )
                        ) {
                            return Err(ArtifactRejection::UnsafePath);
                        }
                        directx
                            .open_file(Path::new(&directx_name))
                            .map_err(|_| ArtifactRejection::UnsafePath)?;
                    }
                    continue;
                }
                if !matches!(
                    payload_name.to_str(),
                    Some("Installer.exe" | "Installer Tome.mpq")
                ) {
                    return Err(ArtifactRejection::UnsafePath);
                }
                payload
                    .open_file(Path::new(&payload_name))
                    .map_err(|_| ArtifactRejection::UnsafePath)?;
            }
            continue;
        }
        if !ALLOWED.iter().any(|allowed| name == OsStr::new(allowed))
            && !is_test_fixture_artifact(&name)
        {
            return Err(ArtifactRejection::UnsafePath);
        }
        cache
            .open_file(Path::new(&name))
            .map_err(|_| ArtifactRejection::UnsafePath)?;
    }
    Ok(())
}

#[cfg(test)]
fn is_test_fixture_artifact(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            "ManagedRuntime.artifact"
                | "Gecko.artifact"
                | "WineMono.artifact"
                | "DotnetDesktop.artifact"
                | "BaseGameWrapper.artifact"
                | "ExpansionWrapper.artifact"
                | "ClientBootstrap.artifact"
                | "FexPackage.artifact"
                | "FexRootFs.artifact"
        )
    )
}

#[cfg(not(test))]
fn is_test_fixture_artifact(_name: &OsStr) -> bool {
    false
}

fn check_host(host: HostSupport) -> Result<(), InstallError> {
    match host {
        HostSupport::Supported => Ok(()),
        HostSupport::Unsupported => Err(InstallError::UnsupportedHost),
        HostSupport::Indeterminate => Err(InstallError::HostEvidenceUnavailable),
    }
}

fn check_recovery_status(status: InstallationStatus) -> Result<(), InstallError> {
    match status {
        InstallationStatus::RepairRequired => Err(InstallError::RepairRequired),
        InstallationStatus::Indeterminate => Err(InstallError::Indeterminate),
        InstallationStatus::StageInterrupted(_) => Err(InstallError::StageInterrupted),
        _ => Ok(()),
    }
}

struct MutationLease(File);

impl MutationLease {
    fn acquire(paths: &ManagedPaths) -> Result<Self, InstallError> {
        let file = paths
            .directory()
            .open_or_create_file(OsStr::new(".pd2launcher.lock"))
            .map_err(|_| InstallError::InstallationBusy)?;
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Err(InstallError::InstallationBusy);
        }
        File::try_lock(&file).map_err(|_| InstallError::InstallationBusy)?;
        Ok(Self(file))
    }
}

impl Drop for MutationLease {
    fn drop(&mut self) {
        let _ = File::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, fs, os::unix::fs::PermissionsExt, path::Path};

    use tempfile::TempDir;

    use super::*;

    /// A launcher folder copied from the other host row (an x86-64 PC's onto
    /// an ARM64 device, or back) keeps that row's runtime download: it is
    /// tolerated and never used, so the cache still serves this row.
    #[test]
    fn the_cache_inventory_accepts_either_host_rows_runtime_download() {
        let fixture = tempfile::tempdir().unwrap();
        let cache = fixture.path().join("cache/artifacts");
        std::fs::create_dir_all(&cache).unwrap();
        for runtime in [
            &crate::runtime::WINE_GE_8_26,
            &crate::runtime::WINE_WOW64_11_11,
            &crate::runtime::WINE_WOW64_11_18,
        ] {
            std::fs::write(cache.join(runtime.cache_name), b"runtime").unwrap();
        }
        assert_eq!(
            validate_artifact_inventory(&Directory::open(&cache).unwrap()),
            Ok(())
        );
    }
    use crate::audit::{CheckStatus, HostSupport};

    const FIXTURE_SHA256: &str = "f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d";

    #[test]
    fn managed_runtime_root_creation_rejects_a_symlink_ancestor() {
        let fixture = TempDir::new().unwrap();
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.path().join("redirect")).unwrap();

        assert!(matches!(
            ManagedPaths::prepare(&fixture.path().join("redirect/managed")),
            Err(InstallError::ManagedRootInvalid)
        ));
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn fresh_authorization_reaches_ready_only_through_durable_evidence() {
        let fixture = TempDir::new().expect("fixture");
        let auditor = ScriptedAuditor::new(fresh_journey());
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .expect("valid authorization");
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        let outcome = installer
            .run(fixture.path(), Some(&authorization))
            .expect("evidence-driven installation should finish");

        assert_eq!(outcome.final_status(), InstallationStatus::Ready);
        assert_eq!(
            outcome.completed_stages(),
            [
                InstallStage::BaseGame,
                InstallStage::Expansion,
                InstallStage::ClientBootstrap,
                InstallStage::ClientSynchronization,
            ]
        );
        // Download fetches Blizzard's installers without a key (setting
        // up Wine for their downloader first); the install steps run from
        // the verified downloads.
        assert_eq!(
            driver.operations.borrow().as_slice(),
            [
                RecordedOperation::ManagedRuntime,
                RecordedOperation::BaseGameDownload,
                RecordedOperation::ExpansionDownload,
                RecordedOperation::BaseGame,
                RecordedOperation::Expansion,
                RecordedOperation::ClientBootstrap,
            ]
        );
        assert_eq!(*client.prefetches.borrow(), 1);
        assert_eq!(*client.calls.borrow(), 1);
        assert_eq!(
            source.acquisitions.borrow().as_slice(),
            [
                // Download.
                ArtifactKind::ManagedRuntime,
                ArtifactKind::Gecko,
                ArtifactKind::WineMono,
                ArtifactKind::DotnetDesktop,
                ArtifactKind::BaseGameWrapper,
                ArtifactKind::ManagedRuntime,
                ArtifactKind::Gecko,
                ArtifactKind::WineMono,
                ArtifactKind::DotnetDesktop,
                ArtifactKind::ExpansionWrapper,
                ArtifactKind::ClientBootstrap,
                // Install Diablo II: never a patch.
                ArtifactKind::BaseGameWrapper,
                ArtifactKind::ExpansionWrapper,
                // Install Project Diablo 2.
                ArtifactKind::ClientBootstrap,
            ]
        );
        assert!(driver.received_base_secret.borrow().as_ref().is_some());
        assert!(driver.received_expansion_secret.borrow().as_ref().is_some());
        assert_eq!(auditor.remaining(), 0);
    }

    #[test]
    fn prefix_and_vendor_stages_are_marked_until_their_evidence_is_proven() {
        use crate::stage_marker::{MarkerState, read};
        let fixture = TempDir::new().expect("fixture");
        let auditor = ScriptedAuditor::new(fresh_journey());
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .unwrap();
        ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
            .run(fixture.path(), Some(&authorization))
            .expect("installation");
        // A downloader run changes no game file and needs no marker.
        assert_eq!(
            *driver.markers.borrow(),
            [
                MarkerState::Present(InterruptedStage::Prefix),
                MarkerState::Absent,
                MarkerState::Absent,
                MarkerState::Present(InterruptedStage::BaseGame),
                MarkerState::Present(InterruptedStage::Expansion),
            ]
        );
        let root = Directory::open(fixture.path()).unwrap();
        assert_eq!(read(&root), MarkerState::Absent);

        // A failed Vendor installer leaves its marker for the next audit.
        let auditor = ScriptedAuditor::new([
            audit(InstallationStatus::NeedsExpansion, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsExpansion, CheckStatus::Satisfied),
        ]);
        let driver = RecordingDriver {
            fail_expansion: true,
            ..RecordingDriver::default()
        };
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .run(fixture.path(), Some(&authorization)),
            Err(InstallError::StageFailed {
                stage: InstallStage::Expansion
            })
        );
        assert_eq!(
            read(&root),
            MarkerState::Present(InterruptedStage::Expansion)
        );

        // And an interrupted stage never runs another stage.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::StageInterrupted(InterruptedStage::Expansion),
            CheckStatus::Satisfied,
        )]);
        let driver = RecordingDriver::default();
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .run(fixture.path(), Some(&authorization)),
            Err(InstallError::StageInterrupted)
        );
        assert!(driver.operations.borrow().is_empty());
    }

    #[test]
    fn a_vendor_error_dialog_is_reported_as_its_own_stage_failure() {
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .unwrap();
        let client = RecordingClientSynchronizer::default();
        for (failure, expected) in [
            (
                VendorFailure::KeyRejected,
                InstallError::KeyRejected {
                    stage: InstallStage::Expansion,
                },
            ),
            (
                VendorFailure::ErrorDialog,
                InstallError::InstallerError {
                    stage: InstallStage::Expansion,
                },
            ),
            (
                VendorFailure::InstallerMenu,
                InstallError::InstallerMenu {
                    stage: InstallStage::Expansion,
                },
            ),
            // Its download, not its installer, failed.
            (
                VendorFailure::DownloadStalled,
                InstallError::DownloaderFailed {
                    stage: InstallStage::ExpansionDownload,
                    failure: DownloaderFailure::Stalled,
                },
            ),
        ] {
            let fixture = TempDir::new().unwrap();
            let auditor = ScriptedAuditor::new([
                audit(InstallationStatus::NeedsExpansion, CheckStatus::Satisfied),
                audit(InstallationStatus::NeedsExpansion, CheckStatus::Satisfied),
            ]);
            let source = FixtureArtifactSource::default();
            let driver = RecordingDriver {
                fail_expansion: true,
                expansion_failure: Some(failure),
                ..RecordingDriver::default()
            };
            assert_eq!(
                ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                    .run(fixture.path(), Some(&authorization)),
                Err(expected)
            );
            // The installed Diablo II is untouched: the next audit decides
            // whether Lord of Destruction can simply run again.
            assert!(
                !driver
                    .operations
                    .borrow()
                    .contains(&RecordedOperation::BaseGame)
            );
        }
    }

    #[test]
    fn a_failed_wrapper_download_refetches_the_blizzard_wrapper_once() {
        let client = RecordingClientSynchronizer::default();
        let base_acquisitions = |source: &FixtureArtifactSource| {
            source
                .acquisitions
                .borrow()
                .iter()
                .filter(|kind| **kind == ArtifactKind::BaseGameWrapper)
                .count()
        };
        let download = |auditor: &ScriptedAuditor,
                        source: &FixtureArtifactSource,
                        driver: &RecordingDriver,
                        catalog: ArtifactCatalog| {
            let fixture = TempDir::new().unwrap();
            ManagedInstaller::new(auditor, source, driver, &client, catalog).download_all(
                fixture.path(),
                DownloadPlan::Installers,
                &OperationControl::default(),
            )
        };

        // An expired wrapper: the download runs again with a fresh one.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver {
            wrapper_download_failures: 1.into(),
            ..RecordingDriver::default()
        };
        assert!(download(&auditor, &source, &driver, fixture_catalog()).is_ok());
        assert_eq!(base_acquisitions(&source), 2);
        assert_eq!(
            driver.operations.borrow()[0],
            RecordedOperation::BaseGameDownload
        );

        // A stalled download lost its connection: the wrapper is kept and
        // the stage fails at once.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver {
            wrapper_download_stalls: 1.into(),
            ..RecordingDriver::default()
        };
        assert_eq!(
            download(&auditor, &source, &driver, fixture_catalog()),
            Err(InstallError::DownloaderFailed {
                stage: InstallStage::BaseGameDownload,
                failure: DownloaderFailure::Stalled,
            })
        );
        assert_eq!(base_acquisitions(&source), 1);

        // Only once: a second failure is reported.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver {
            wrapper_download_failures: 2.into(),
            ..RecordingDriver::default()
        };
        assert_eq!(
            download(&auditor, &source, &driver, fixture_catalog()),
            Err(InstallError::DownloaderFailed {
                stage: InstallStage::BaseGameDownload,
                failure: DownloaderFailure::Ended,
            })
        );
        assert_eq!(base_acquisitions(&source), 2);

        // An error box, an unfinished download and other files are retried
        // with a fresh wrapper too, then reported as what they were.
        for (failure, expected) in [
            (
                VendorFailure::DownloaderErrorDialog,
                DownloaderFailure::ErrorDialog,
            ),
            (
                VendorFailure::PayloadIncomplete,
                DownloaderFailure::Incomplete,
            ),
            (VendorFailure::PayloadRejected, DownloaderFailure::Rejected),
        ] {
            let auditor = ScriptedAuditor::new([audit(
                InstallationStatus::NeedsBaseGame,
                CheckStatus::Satisfied,
            )]);
            let source = FixtureArtifactSource::default();
            let driver = RecordingDriver {
                wrapper_download_failures: 2.into(),
                wrapper_download_failure: Some(failure).into(),
                ..RecordingDriver::default()
            };
            assert_eq!(
                download(&auditor, &source, &driver, fixture_catalog()),
                Err(InstallError::DownloaderFailed {
                    stage: InstallStage::BaseGameDownload,
                    failure: expected,
                })
            );
            assert_eq!(base_acquisitions(&source), 2);
        }

        // A custom source is never replaced: its failure is reported.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver {
            wrapper_download_failures: 1.into(),
            ..RecordingDriver::default()
        };
        let catalog = ArtifactCatalog::new([
            fixed_spec(ArtifactKind::ManagedRuntime),
            fixed_spec(ArtifactKind::Gecko),
            fixed_spec(ArtifactKind::WineMono),
            fixed_spec(ArtifactKind::DotnetDesktop),
            ArtifactSpec {
                request_url: Cow::Borrowed("https://mirror.invalid/d2.exe"),
                redirects: RedirectPolicy::AnyHttp,
                size: SizePolicy::Bounded { min: 1, max: 256 },
                integrity: IntegrityPolicy::Unverified,
                expected_filename: None,
                ..wrapper_spec(ArtifactKind::BaseGameWrapper)
            },
            wrapper_spec(ArtifactKind::ExpansionWrapper),
            fixed_spec(ArtifactKind::ClientBootstrap),
            fixed_spec(ArtifactKind::FexPackage),
            fixed_spec(ArtifactKind::FexRootFs),
        ]);
        assert_eq!(
            download(&auditor, &source, &driver, catalog),
            Err(InstallError::DownloaderFailed {
                stage: InstallStage::BaseGameDownload,
                failure: DownloaderFailure::Ended,
            })
        );
        assert_eq!(base_acquisitions(&source), 1);
    }

    #[test]
    fn start_over_removes_only_the_prefix_the_earlier_runtime_and_the_stage_marker_under_the_lease()
    {
        let fixture = TempDir::new().unwrap();
        let root_path = fixture.path();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("keep"), b"outside").unwrap();
        let current = format!("runner/{}/bin", crate::runtime::current().identity);
        for directory in [
            current.as_str(),
            "runner/wine-11.18-amd64-wow64/bin",
            "cache/artifacts",
            "cache/xdg",
            "prefix/drive_c/Program Files (x86)/Diablo II/Save",
            "prefix/dosdevices",
        ] {
            fs::create_dir_all(root_path.join(directory)).unwrap();
        }
        fs::write(root_path.join(&current).join("wine"), b"wine").unwrap();
        fs::write(
            root_path.join("runner/wine-11.18-amd64-wow64/bin/wine"),
            b"earlier wine",
        )
        .unwrap();
        fs::write(
            root_path.join("cache/artifacts/wine-11.18-amd64-wow64.verified"),
            b"earlier archive",
        )
        .unwrap();
        fs::write(
            root_path.join("cache/artifacts/artifact-01.partial"),
            b"half",
        )
        .unwrap();
        fs::write(
            root_path.join("prefix/drive_c/Program Files (x86)/Diablo II/d2data.mpq"),
            b"partial",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), root_path.join("prefix/dosdevices/z:")).unwrap();
        // A directory Wine marked read-only.
        let read_only = root_path.join("prefix/drive_c/Program Files (x86)/Diablo II/Save");
        fs::write(read_only.join("hero.d2s"), b"save").unwrap();
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o500)).unwrap();
        let root = Directory::open(root_path).unwrap();
        crate::stage_marker::write(&root, InterruptedStage::BaseGame).unwrap();

        let paths = ManagedPaths::prepare(root_path).unwrap();
        // Another holder of the lease blocks the reset entirely.
        let other = MutationLease::acquire(&paths).unwrap();
        assert_eq!(
            MutationLease::acquire(&paths).err(),
            Some(InstallError::InstallationBusy)
        );
        // A file that is not this root's lock is refused.
        let stranger = File::open(outside.path().join("keep")).unwrap();
        assert!(reset_managed_prefix(&root, &stranger).is_err());
        assert!(root_path.join("prefix").is_dir());

        reset_managed_prefix(&root, &other.0).expect("reset under the lease");
        drop(other);
        assert!(!root_path.join("prefix").exists());
        assert_eq!(
            crate::stage_marker::read(&root),
            crate::stage_marker::MarkerState::Absent
        );
        assert_eq!(
            fs::read(root_path.join(&current).join("wine")).unwrap(),
            b"wine"
        );
        // The x86-64 row's earlier runtime served only the removed prefix.
        assert_eq!(
            root_path.join("runner/wine-11.18-amd64-wow64").exists(),
            crate::runtime::retained().is_none()
        );
        // Downloads are kept.
        assert!(
            root_path
                .join("cache/artifacts/wine-11.18-amd64-wow64.verified")
                .is_file()
        );
        assert_eq!(
            fs::read(root_path.join("cache/artifacts/artifact-01.partial")).unwrap(),
            b"half"
        );
        assert!(root_path.join("cache/xdg").is_dir());
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"outside");
        // A second Start over has nothing left to remove.
        let lease = MutationLease::acquire(&paths).unwrap();
        reset_managed_prefix(&root, &lease.0).unwrap();
    }

    /// A source that stops the job when it is asked for `stop_at`.
    struct StoppingSource<'a> {
        inner: &'a FixtureArtifactSource,
        stop_at: ArtifactKind,
    }

    impl ArtifactSource for StoppingSource<'_> {
        fn acquire_at(
            &self,
            spec: &ArtifactSpec,
            cache: &Path,
            cache_directory: &Directory,
            control: &OperationControl,
        ) -> Result<AcquiredArtifact, AdapterFailure> {
            if spec.kind == self.stop_at {
                control.request_cancel();
                return Err(AdapterFailure::here());
            }
            self.inner.acquire_at(spec, cache, cache_directory, control)
        }
    }

    #[test]
    fn a_chosen_built_in_fex_is_downloaded_and_installed_before_anything_else() {
        let fixture = TempDir::new().unwrap();
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let auditor =
            ScriptedAuditor::new([
                audit(InstallationStatus::NeedsBaseGame, CheckStatus::Missing)
                    .with_builtin_fex_needed(true),
            ]);
        // The fixture bytes are no `.deb`, so installing them stops.
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .download_all(
                    fixture.path(),
                    DownloadPlan::Installers,
                    &OperationControl::default()
                ),
            Err(InstallError::StageFailed {
                stage: InstallStage::BuiltinFex
            })
        );
        assert_eq!(
            *source.acquisitions.borrow(),
            [ArtifactKind::FexPackage, ArtifactKind::FexRootFs],
            "nothing else, Blizzard's downloader included, runs before it"
        );
        assert!(!crate::builtin_fex::installed(fixture.path()));
    }

    /// The Android host's audit, as `ProductionAuditor` reports it there:
    /// the app provides the prefix's .NET Desktop Runtime (for the official
    /// PD2 launcher), and the prefix never gets Wine Gecko or Wine Mono,
    /// whatever the audit of the prefix says.
    fn android_audit(status: InstallationStatus, runtime: CheckStatus) -> InstallAudit {
        audit(status, runtime)
            .with_prefix_needs(PrefixNeeds {
                gecko: true,
                mono: true,
                dotnet: false,
            })
            .with_android_host()
    }

    #[test]
    fn on_android_blizzards_downloaders_run_in_install_not_in_download() {
        use crate::downloads::{DownloadReadiness, readiness};
        let fixture = TempDir::new().unwrap();
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let auditor = ScriptedAuditor::new([
            // Download.
            android_audit(InstallationStatus::NeedsBaseGame, CheckStatus::Missing),
            // Install Diablo II, after the runtime, after each installer.
            android_audit(InstallationStatus::NeedsBaseGame, CheckStatus::Missing),
            android_audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            android_audit(InstallationStatus::NeedsExpansion, CheckStatus::Satisfied),
            android_audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            // Install Project Diablo 2.
            android_audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            android_audit(InstallationStatus::Ready, CheckStatus::Satisfied),
        ]);
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());
        installer
            .download_all(
                fixture.path(),
                DownloadPlan::Installers,
                &OperationControl::default(),
            )
            .expect("downloads verified");
        // Download keeps Blizzard's wrappers, verified, and runs nothing: no
        // Wine (the app provides it, its prefix and the prefix's .NET), no
        // downloader, and no Wine Gecko or Wine Mono. The readiness check
        // counts no record of what the app provides there.
        assert_eq!(
            *source.acquisitions.borrow(),
            [
                ArtifactKind::BaseGameWrapper,
                ArtifactKind::ExpansionWrapper,
                ArtifactKind::ClientBootstrap,
            ]
        );
        assert!(
            source
                .acquisitions
                .borrow()
                .iter()
                .all(|kind| !crate::downloads::PROVIDED_ON_ANDROID.contains(kind))
        );
        assert!(driver.operations.borrow().is_empty());
        assert!(matches!(
            readiness(&Directory::open(fixture.path()).unwrap()),
            DownloadReadiness::Complete {
                plan: DownloadPlan::Installers,
                ..
            }
        ));
        let receipt = fs::read_to_string(fixture.path().join("cache/downloads.json")).unwrap();
        for wrapper in ["BaseGameWrapper", "ExpansionWrapper"] {
            assert!(receipt.contains(&format!("cache/artifacts/{wrapper}.artifact")));
        }
        assert!(!receipt.contains("payload"));

        // Install Diablo II: Wine, then each of Blizzard's downloaders right
        // before its installer.
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .unwrap();
        let outcome = installer
            .install_base_game(
                fixture.path(),
                Some(&authorization),
                &OperationControl::default(),
            )
            .unwrap();
        assert_eq!(outcome.final_status(), InstallationStatus::NeedsClient);
        assert_eq!(
            driver.operations.borrow().as_slice(),
            [
                RecordedOperation::ManagedRuntime,
                RecordedOperation::BaseGameDownload,
                RecordedOperation::BaseGame,
                RecordedOperation::ExpansionDownload,
                RecordedOperation::Expansion,
            ]
        );
        let ready = installer
            .install_project_diablo(fixture.path(), &OperationControl::default())
            .unwrap();
        assert_eq!(ready.final_status(), InstallationStatus::Ready);
        assert_eq!(auditor.remaining(), 0);
    }

    #[test]
    fn download_all_records_its_result_only_when_everything_verified() {
        use crate::downloads::{DownloadReadiness, readiness};
        let fixture = TempDir::new().unwrap();
        let root = || Directory::open(fixture.path()).unwrap();
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();

        // A stop part way reports a safe cancellation and records nothing.
        let auditor = ScriptedAuditor::new([
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Missing),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
        ]);
        let stopping = StoppingSource {
            inner: &source,
            stop_at: ArtifactKind::ClientBootstrap,
        };
        assert_eq!(
            ManagedInstaller::new(&auditor, &stopping, &driver, &client, fixture_catalog())
                .download_all(
                    fixture.path(),
                    DownloadPlan::Installers,
                    &OperationControl::default()
                ),
            Err(InstallError::Cancelled)
        );
        assert_eq!(readiness(&root()), DownloadReadiness::Incomplete);
        assert_eq!(*client.prefetches.borrow(), 0);

        // Downloading again finishes what's missing and records the result.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        let outcome = ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
            .download_all(
                fixture.path(),
                DownloadPlan::Installers,
                &OperationControl::default(),
            )
            .expect("downloads verified");
        assert!(outcome.total_bytes > 0);
        let complete = readiness(&root());
        assert_eq!(
            complete,
            DownloadReadiness::Complete {
                plan: DownloadPlan::Installers,
                total_bytes: outcome.total_bytes,
                reach: crate::downloads::DownloadReach::BaseGame,
                prefix: false,
            }
        );
        assert_eq!(*client.prefetches.borrow(), 1);
        // Nothing was installed: no key was needed, no game stage ran.
        assert!(driver.received_base_secret.borrow().is_none());
        assert!(!driver.operations.borrow().iter().any(|operation| matches!(
            operation,
            RecordedOperation::BaseGame | RecordedOperation::Expansion
        )));

        // For an existing installation, Blizzard's installers are not
        // downloaded, and no Wine prefix is set up yet.
        let fixture = TempDir::new().unwrap();
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Missing,
        )]);
        ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
            .download_all(
                fixture.path(),
                DownloadPlan::ExistingInstall,
                &OperationControl::default(),
            )
            .expect("downloads verified");
        assert_eq!(
            *source.acquisitions.borrow(),
            [
                ArtifactKind::ManagedRuntime,
                ArtifactKind::Gecko,
                ArtifactKind::WineMono,
                ArtifactKind::DotnetDesktop,
                ArtifactKind::ClientBootstrap,
            ]
        );
        assert!(driver.operations.borrow().is_empty());
        let existing = readiness(&Directory::open(fixture.path()).unwrap());
        let fresh =
            crate::downloads::DownloadNeed::for_status(InstallationStatus::NeedsBaseGame, true);
        assert!(existing.serves(DownloadPlan::ExistingInstall, fresh));
        assert!(!existing.serves(DownloadPlan::Installers, fresh));
    }

    #[test]
    fn the_download_job_says_it_downloads_and_the_install_jobs_that_they_install() {
        use std::sync::{Arc, Mutex};
        let phases = Arc::new(Mutex::new(Vec::new()));
        let control = OperationControl::default();
        let seen = Arc::clone(&phases);
        control.observe(Arc::new(move |progress: WorkProgress| {
            seen.lock().unwrap().push(progress.phase());
        }));
        let fixture = TempDir::new().unwrap();
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let auditor = ScriptedAuditor::new(fresh_journey());
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .unwrap();
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());
        installer
            .download_all(fixture.path(), DownloadPlan::Installers, &control)
            .unwrap();
        let downloading = std::mem::take(&mut *phases.lock().unwrap());
        for phase in [
            WorkPhase::RuntimeAcquisition,
            WorkPhase::BaseGameDownload,
            WorkPhase::ExpansionDownload,
            WorkPhase::ClientDownload,
        ] {
            assert!(downloading.contains(&phase), "{phase:?}");
        }
        for phase in [WorkPhase::BaseGameInstall, WorkPhase::ExpansionInstall] {
            assert!(!downloading.contains(&phase), "{phase:?} while downloading");
        }
        installer
            .install_base_game(fixture.path(), Some(&authorization), &control)
            .unwrap();
        let installing = std::mem::take(&mut *phases.lock().unwrap());
        for phase in [WorkPhase::BaseGameInstall, WorkPhase::ExpansionInstall] {
            assert!(installing.contains(&phase), "{phase:?}");
        }
        for phase in [
            WorkPhase::RuntimeAcquisition,
            WorkPhase::BaseGameDownload,
            WorkPhase::ExpansionDownload,
        ] {
            assert!(!installing.contains(&phase), "{phase:?} while installing");
        }
    }

    #[test]
    fn each_install_step_runs_only_at_its_turn() {
        let fixture = TempDir::new().unwrap();
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        // Project Diablo 2 needs Diablo II first.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .install_project_diablo(fixture.path(), &OperationControl::default()),
            Err(InstallError::BaseGameRequired)
        );
        // Diablo II installed: nothing is left for Install Diablo II.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsClient,
            CheckStatus::Satisfied,
        )]);
        let outcome = ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
            .install_base_game(fixture.path(), None, &OperationControl::default())
            .unwrap();
        assert_eq!(outcome.final_status(), InstallationStatus::NeedsClient);
        assert!(outcome.completed_stages().is_empty());
        // Diablo II's installers need both keys.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .install_base_game(fixture.path(), None, &OperationControl::default()),
            Err(InstallError::AuthorizationRequired)
        );
        assert!(driver.operations.borrow().is_empty());
        assert_eq!(*client.calls.borrow(), 0);
    }

    fn existing_install(fixture: &TempDir) -> ExistingInstall {
        let folder = fixture.path().join("My Games/Diablo II");
        crate::game_folder::tests::write_game_folder(&folder, b"game-1.14d", b"archive-1.14d");
        crate::game_folder::validate_existing_with(
            &folder,
            &fixture.path().join("managed"),
            &[crate::game_folder::tests::pinned_version(
                b"game-1.14d",
                b"archive-1.14d",
            )],
        )
        .expect("a 1.14d-shaped folder")
    }

    #[test]
    fn an_existing_installation_is_imported_without_keys_under_a_stage_marker() {
        use crate::stage_marker::{MarkerState, read};
        let fixture = TempDir::new().unwrap();
        let existing = existing_install(&fixture);
        let root_path = fixture.path().join("managed");
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let auditor = ScriptedAuditor::new([
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Missing),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
        ]);
        let outcome = ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
            .import_existing(
                &root_path,
                &existing,
                ImportMode::InPlace,
                &OperationControl::default(),
            )
            .expect("imported");
        assert_eq!(outcome.final_status(), InstallationStatus::NeedsClient);
        assert_eq!(
            outcome.completed_stages(),
            [InstallStage::ManagedRuntime, InstallStage::Import]
        );
        assert_eq!(
            *driver.operations.borrow(),
            [
                RecordedOperation::ManagedRuntime,
                RecordedOperation::Import(ImportMode::InPlace)
            ]
        );
        // The import ran under its marker, cleared once proven; no key.
        assert_eq!(
            driver.markers.borrow().last(),
            Some(&MarkerState::Present(InterruptedStage::BaseGame))
        );
        assert_eq!(
            read(&Directory::open(&root_path).unwrap()),
            MarkerState::Absent
        );
        assert!(driver.received_base_secret.borrow().is_none());

        // A failed copy leaves its marker: only Start over continues.
        let driver = RecordingDriver {
            fail_import: true,
            ..RecordingDriver::default()
        };
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .import_existing(
                    &root_path,
                    &existing,
                    ImportMode::Copy,
                    &OperationControl::default()
                ),
            Err(InstallError::StageFailed {
                stage: InstallStage::Import
            })
        );
        assert_eq!(
            read(&Directory::open(&root_path).unwrap()),
            MarkerState::Present(InterruptedStage::BaseGame)
        );

        // Nothing is imported over a game that is already there, or from a
        // folder that stopped being usable.
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsExpansion,
            CheckStatus::Satisfied,
        )]);
        assert_eq!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .import_existing(
                    &root_path,
                    &existing,
                    ImportMode::Copy,
                    &OperationControl::default()
                ),
            Err(InstallError::BaseGameInstalled)
        );
        fs::write(existing.path().join("Game.exe"), b"game-1.14b").unwrap();
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        assert!(matches!(
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .import_existing(
                    &root_path,
                    &existing,
                    ImportMode::Copy,
                    &OperationControl::default()
                ),
            Err(InstallError::ExistingInstallInvalid(_))
        ));
    }

    #[test]
    fn on_android_a_folder_used_in_place_with_its_project_diablo_skips_the_pd2_installer() {
        let fixture = TempDir::new().unwrap();
        let existing = existing_install(&fixture);
        let root_path = fixture.path().join("managed");
        let source = FixtureArtifactSource::default();
        let client = RecordingClientSynchronizer::default();
        let import = |audits: [InstallAudit; 2], mode| {
            let driver = RecordingDriver::default();
            let auditor = ScriptedAuditor::new(audits);
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                .import_existing(&root_path, &existing, mode, &OperationControl::default())
                .map(|outcome| outcome.final_status())
        };
        let satisfied = |status, android: bool| {
            let report = audit(status, CheckStatus::Satisfied);
            if android {
                report.with_android_host()
            } else {
                report
            }
        };

        // The copy's ProjectD2 is accepted: Ready, and nothing is installed.
        assert_eq!(
            import(
                [
                    satisfied(InstallationStatus::NeedsBaseGame, true),
                    satisfied(InstallationStatus::Ready, true),
                ],
                ImportMode::InPlace,
            ),
            Ok(InstallationStatus::Ready)
        );
        assert_eq!(*client.calls.borrow(), 0);
        // Without one (or with one the audit refuses) PD2 is installed next.
        assert_eq!(
            import(
                [
                    satisfied(InstallationStatus::NeedsBaseGame, true),
                    satisfied(InstallationStatus::NeedsClient, true),
                ],
                ImportMode::InPlace,
            ),
            Ok(InstallationStatus::NeedsClient)
        );
        // The desktop accepts it too, from a folder used in place and from
        // a copy, which brings ProjectD2 along.
        for mode in [ImportMode::InPlace, ImportMode::Copy] {
            assert_eq!(
                import(
                    [
                        satisfied(InstallationStatus::NeedsBaseGame, false),
                        satisfied(InstallationStatus::Ready, false),
                    ],
                    mode,
                ),
                Ok(InstallationStatus::Ready),
                "{mode:?}"
            );
        }
        // An Android engine's copy (which leaves ProjectD2 out) is
        // unchanged: a Ready after the import is unexplained evidence.
        assert!(matches!(
            import(
                [
                    satisfied(InstallationStatus::NeedsBaseGame, true),
                    satisfied(InstallationStatus::Ready, true),
                ],
                ImportMode::Copy,
            ),
            Err(InstallError::EvidenceDidNotAdvance { .. })
        ));
    }

    #[test]
    fn on_android_a_project_diablo_in_the_game_folder_is_updated_in_place_without_the_pd2_installer()
     {
        let source = FixtureArtifactSource::default();
        // `project` lists the ProjectD2 files the game folder holds.
        let install = |project: &[&str], android: bool, audits: Vec<InstallationStatus>| {
            let fixture = TempDir::new().unwrap();
            let project_d2 = fixture
                .path()
                .join("prefix/drive_c/Program Files (x86)/Diablo II/ProjectD2");
            fs::create_dir_all(&project_d2).unwrap();
            for file in project {
                fs::write(project_d2.join(file), b"pd2").unwrap();
            }
            let driver = RecordingDriver::default();
            let client = RecordingClientSynchronizer::default();
            let auditor = ScriptedAuditor::new(audits.into_iter().map(|status| {
                let report = audit(status, CheckStatus::Satisfied);
                if android {
                    report.with_android_host()
                } else {
                    report
                }
            }));
            let outcome =
                ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog())
                    .install_project_diablo(fixture.path(), &OperationControl::default())
                    .unwrap();
            assert_eq!(auditor.remaining(), 0);
            assert_eq!(outcome.final_status(), InstallationStatus::Ready);
            let bootstrap = driver
                .operations
                .borrow()
                .contains(&RecordedOperation::ClientBootstrap);
            let synchronized = *client.calls.borrow();
            (bootstrap, synchronized, outcome.completed_stages().to_vec())
        };
        let complete = ["Game.exe", "ProjectDiablo.dll"];
        let installer = [
            InstallStage::ClientBootstrap,
            InstallStage::ClientSynchronization,
        ];

        // Adopted: the updater brings it to Ready, the PD2 installer never
        // runs.
        assert_eq!(
            install(
                &complete,
                true,
                vec![InstallationStatus::NeedsClient, InstallationStatus::Ready]
            ),
            (false, 1, vec![InstallStage::ClientSynchronization])
        );
        // Unusable (no client launch files): the PD2 installer runs.
        assert_eq!(
            install(
                &["Game.exe"],
                true,
                vec![InstallationStatus::NeedsClient, InstallationStatus::Ready]
            ),
            (true, 1, installer.to_vec())
        );
        // Not Ready once updated: the PD2 installer runs, then the update.
        assert_eq!(
            install(
                &complete,
                true,
                vec![
                    InstallationStatus::NeedsClient,
                    InstallationStatus::NeedsClient,
                    InstallationStatus::Ready,
                ]
            ),
            (true, 2, installer.to_vec())
        );
        // The desktop adopts it the same way, and runs the PD2 installer
        // where there is nothing to adopt.
        assert_eq!(
            install(
                &complete,
                false,
                vec![InstallationStatus::NeedsClient, InstallationStatus::Ready]
            ),
            (false, 1, vec![InstallStage::ClientSynchronization])
        );
        assert_eq!(
            install(
                &[],
                false,
                vec![InstallationStatus::NeedsClient, InstallationStatus::Ready]
            ),
            (true, 1, installer.to_vec())
        );
    }

    #[test]
    fn start_over_never_touches_a_game_folder_used_in_place() {
        use std::os::unix::fs::MetadataExt;
        let fixture = TempDir::new().unwrap();
        let root_path = fixture.path().join("managed");
        fs::create_dir_all(root_path.join("prefix/drive_c/windows")).unwrap();
        let external = fixture.path().join("My Games/Diablo II");
        crate::game_folder::tests::write_game_folder(&external, b"game", b"archive");
        fs::write(external.join("ProjectD2/loot.filter"), b"mine").unwrap();
        let metadata = fs::metadata(&external).unwrap();
        fs::write(
            root_path
                .join("prefix")
                .join(crate::game_folder::POINTER_NAME),
            format!(
                r#"{{"schema":1,"path":"{}","filesystem":{},"inode":{}}}"#,
                external.display(),
                rustix::fs::statvfs(&external).unwrap().f_fsid,
                metadata.ino()
            ),
        )
        .unwrap();
        // Wine's own link to the host root stays a link: never followed.
        fs::create_dir_all(root_path.join("prefix/dosdevices")).unwrap();
        std::os::unix::fs::symlink("/", root_path.join("prefix/dosdevices/z:")).unwrap();
        let root = Directory::open(&root_path).unwrap();
        assert!(crate::game_folder::GameFolder::resolve(&root).is_ok());

        let paths = ManagedPaths::prepare(&root_path).unwrap();
        let lease = MutationLease::acquire(&paths).unwrap();
        reset_managed_prefix(&root, &lease.0).expect("start over");
        drop(lease);
        assert!(!root_path.join("prefix").exists());
        // The folder, and what Project Diablo 2 added to it, are untouched;
        // the launcher simply no longer uses it.
        assert_eq!(fs::read(external.join("Game.exe")).unwrap(), b"game");
        assert_eq!(
            fs::read(external.join("ProjectD2/loot.filter")).unwrap(),
            b"mine"
        );
        assert_eq!(
            crate::game_folder::GameFolder::read(&root),
            Ok(crate::game_folder::GameFolder::Managed)
        );
    }

    #[test]
    fn a_rejected_custom_gecko_is_replaced_without_downloading_the_accepted_runtime() {
        let fixture = TempDir::new().expect("fixture");
        let rejected_gecko = || {
            InstallAudit::new(
                InstallationStatus::NeedsClient,
                HostSupport::Supported,
                CheckStatus::Satisfied,
                CheckStatus::Invalid,
            )
            .with_prefix_needs(PrefixNeeds {
                gecko: true,
                ..PrefixNeeds::default()
            })
        };
        let auditor = ScriptedAuditor::new([
            // Download, Install Diablo II (nothing to do), Install PD2.
            rejected_gecko(),
            rejected_gecko(),
            rejected_gecko(),
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            audit(InstallationStatus::Ready, CheckStatus::Satisfied),
        ]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        let outcome = installer.run(fixture.path(), None).expect("restored");
        assert_eq!(outcome.final_status(), InstallationStatus::Ready);
        // Downloaded once, then used from the cache by the install steps.
        assert_eq!(
            *source.acquisitions.borrow(),
            [
                ArtifactKind::Gecko,
                ArtifactKind::ClientBootstrap,
                ArtifactKind::Gecko,
                ArtifactKind::ClientBootstrap,
            ]
        );
        assert_eq!(
            driver.operations.borrow()[..2],
            [
                RecordedOperation::ManagedRuntime,
                RecordedOperation::ClientBootstrap
            ]
        );
    }

    #[test]
    fn a_complete_installation_missing_wine_components_gets_only_those_in_place() {
        // An installation an older release made without Wine Mono and the
        // .NET Desktop Runtime in its prefix: the game is complete.
        let needs_runtime = || {
            InstallAudit::new(
                InstallationStatus::NeedsRuntime,
                HostSupport::Supported,
                CheckStatus::Satisfied,
                CheckStatus::Missing,
            )
            .with_prefix_needs(PrefixNeeds {
                gecko: false,
                mono: true,
                dotnet: true,
            })
        };
        let fixture = TempDir::new().expect("fixture");
        let auditor = ScriptedAuditor::new([
            needs_runtime(),
            needs_runtime(),
            needs_runtime(),
            audit(InstallationStatus::Ready, CheckStatus::Satisfied),
        ]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        // No product keys are needed, and nothing but the two components is
        // downloaded (then used from the cache); no game stage runs again.
        let outcome = installer.run(fixture.path(), None).expect("migrated");
        assert_eq!(outcome.final_status(), InstallationStatus::Ready);
        assert_eq!(outcome.completed_stages(), [InstallStage::ManagedRuntime]);
        assert_eq!(
            *source.acquisitions.borrow(),
            [
                ArtifactKind::WineMono,
                ArtifactKind::DotnetDesktop,
                ArtifactKind::WineMono,
                ArtifactKind::DotnetDesktop
            ]
        );
        assert_eq!(
            *driver.operations.borrow(),
            [RecordedOperation::ManagedRuntime]
        );
        assert_eq!(*client.calls.borrow(), 0);
        assert_eq!(*client.prefetches.borrow(), 0);
        assert_eq!(auditor.remaining(), 0);

        // A provisioning pass that does not produce Ready fails closed.
        let auditor = ScriptedAuditor::new([
            needs_runtime(),
            needs_runtime(),
            needs_runtime(),
            needs_runtime(),
        ]);
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());
        assert_eq!(
            installer.run(fixture.path(), None),
            Err(InstallError::RuntimeEvidenceInvalid)
        );
    }

    #[test]
    fn successful_process_without_new_evidence_does_not_advance_installation() {
        let fixture = TempDir::new().expect("fixture");
        let auditor = ScriptedAuditor::new([
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
        ]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .expect("valid authorization");
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        let error = installer
            .run(fixture.path(), Some(&authorization))
            .expect_err("process completion is not Installation evidence");

        assert_eq!(
            error,
            InstallError::EvidenceDidNotAdvance {
                stage: InstallStage::BaseGame,
                expected: InstallationStatus::NeedsExpansion,
                observed: InstallationStatus::NeedsBaseGame,
            }
        );
    }

    #[test]
    fn conflicting_evidence_after_a_vendor_stage_fails_closed() {
        let fixture = TempDir::new().expect("fixture");
        let auditor = ScriptedAuditor::new([
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::Indeterminate, CheckStatus::Satisfied),
        ]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .expect("valid authorization");
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        assert_eq!(
            installer.run(fixture.path(), Some(&authorization)),
            Err(InstallError::Indeterminate)
        );
    }

    #[test]
    fn a_cache_left_by_the_retired_patch_stage_is_still_accepted() {
        // An installation an older release patched keeps its cached patch,
        // the stub's log, and a patch download slot: tolerated, never used,
        // and never a reason to stop.
        let fixture = TempDir::new().unwrap();
        let cache = fixture.path().join("cache/artifacts");
        fs::create_dir_all(&cache).unwrap();
        for leftover in [
            "lod-patch-1.14d.verified",
            "custom-patch.download",
            "custom-patch.download.provenance.json",
            "prepatch.log",
            "artifact-04.partial",
        ] {
            fs::write(cache.join(leftover), b"leftover").unwrap();
        }
        assert_eq!(
            validate_artifact_inventory(&Directory::open(&cache).unwrap()),
            Ok(())
        );
        fs::write(cache.join("unexpected.exe"), b"x").unwrap();
        assert_eq!(
            validate_artifact_inventory(&Directory::open(&cache).unwrap()),
            Err(ArtifactRejection::UnsafePath)
        );
    }

    #[test]
    fn proven_vendor_stages_resume_without_product_keys() {
        let fixture = TempDir::new().expect("fixture");
        let auditor = ScriptedAuditor::new([
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            audit(InstallationStatus::Ready, CheckStatus::Satisfied),
        ]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        let outcome = installer
            .run(fixture.path(), None)
            .expect("proven vendor checkpoints do not require fresh keys");

        assert_eq!(outcome.final_status(), InstallationStatus::Ready);
        assert_eq!(
            outcome.completed_stages(),
            [
                InstallStage::ClientBootstrap,
                InstallStage::ClientSynchronization,
            ]
        );
        assert!(driver.received_base_secret.borrow().is_none());
        assert!(driver.received_expansion_secret.borrow().is_none());
    }

    #[test]
    fn authorization_requires_consent_and_never_formats_product_keys() {
        let missing_consent = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            false,
        );
        assert!(matches!(
            missing_consent,
            Err(AuthorizationError::ConsentRequired)
        ));

        let authorization = InstallationAuthorization::new(
            "BASE-CANARY-1234".to_owned(),
            "EXPANSION-CANARY-5678".to_owned(),
            true,
        )
        .expect("valid authorization");
        let rendered = format!("{authorization:?}");
        assert!(!rendered.contains("CANARY"));
        assert_eq!(rendered.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn artifact_verification_rejects_unapproved_origin_and_digest() {
        let fixture = TempDir::new().expect("fixture");
        let cache = fixture.path().join("cache");
        fs::create_dir(&cache).expect("cache");
        let path = cache.join("artifact.bin");
        fs::write(&path, b"fixture").expect("artifact");
        let spec = fixed_spec(ArtifactKind::Gecko);

        let rejected_origin = verify_artifact(
            &spec,
            &cache,
            AcquiredArtifact::new(
                path.clone(),
                spec.request_url(),
                "https://mirror.invalid/artifact.bin",
                None,
            ),
        );
        assert_eq!(rejected_origin, Err(ArtifactRejection::UnapprovedFinalUrl));

        fs::write(&path, b"tamper!").expect("tampered artifact");
        let rejected_digest = verify_artifact(
            &spec,
            &cache,
            AcquiredArtifact::new(path, spec.request_url(), spec.request_url(), None),
        );
        assert_eq!(rejected_digest, Err(ArtifactRejection::DigestMismatch));
    }

    #[test]
    fn wrapper_verification_requires_the_expected_name_and_pe_structure() {
        let fixture = TempDir::new().expect("fixture");
        let cache = fixture.path().join("cache");
        fs::create_dir(&cache).expect("cache");
        let path = cache.join("wrapper.exe");
        fs::write(&path, minimal_pe()).expect("wrapper");
        let spec = wrapper_spec(ArtifactKind::BaseGameWrapper);

        let wrong_name = verify_artifact(
            &spec,
            &cache,
            AcquiredArtifact::new(
                path.clone(),
                spec.request_url(),
                spec.request_url(),
                Some("not-the-approved-wrapper.exe".to_owned()),
            ),
        );
        assert_eq!(wrong_name, Err(ArtifactRejection::UnexpectedFilename));

        fs::write(&path, [0_u8; 128]).expect("invalid executable");
        let invalid_pe = verify_artifact(
            &spec,
            &cache,
            AcquiredArtifact::new(
                path,
                spec.request_url(),
                spec.request_url(),
                spec.expected_filename.map(str::to_owned),
            ),
        );
        assert_eq!(invalid_pe, Err(ArtifactRejection::InvalidExecutable));
    }

    #[test]
    fn complete_payload_verification_retains_the_nested_installer_path() {
        let fixture = TempDir::new().expect("fixture");
        let cache = fixture.path().join("cache");
        let payload = cache.join("d2-payload");
        fs::create_dir_all(&payload).expect("payload cache");
        let path = payload.join("Installer.exe");
        let bytes = minimal_pe();
        fs::write(&path, &bytes).expect("complete installer");
        let spec = wrapper_spec(ArtifactKind::BaseGameWrapper);
        let verified = verify_artifact(
            &spec,
            &cache,
            AcquiredArtifact::user_selected(path.clone(), spec.expected_filename.unwrap())
                .with_retained_receipt_sha256(hex(&Sha256::digest(&bytes))),
        )
        .expect("verified nested complete installer");

        assert_eq!(verified.path(), path);
    }

    #[test]
    fn the_builtin_fex_comes_from_the_mirror_then_launchpad_with_the_same_pins() {
        let catalog = ArtifactCatalog::pinned();
        let package = crate::builtin_fex::FexVariant::current().package();
        let spec = catalog.spec(ArtifactKind::FexPackage);
        let sources = spec
            .sources()
            .map(|source| (source.request_url().to_owned(), source.redirects))
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            [
                (
                    package.mirror_url().to_owned(),
                    RedirectPolicy::GithubReleaseAsset
                ),
                (
                    package.upstream()[0].to_owned(),
                    RedirectPolicy::LaunchpadLibrarian
                ),
                (package.upstream()[1].to_owned(), RedirectPolicy::None),
            ]
        );
        assert!(sources[0].0.starts_with("https://github.com/"));
        assert!(sources[1].0.contains("/+files/"));
        assert!(sources[2].0.contains("/pool/"));
        for source in spec.sources() {
            assert_eq!(source.size, SizePolicy::Exact(package.size()));
            assert_eq!(source.integrity, IntegrityPolicy::Sha256(package.sha256()));
            assert!(source.fallback_urls.is_empty());
        }

        let rootfs = crate::builtin_fex::rootfs();
        let spec = catalog.spec(ArtifactKind::FexRootFs);
        assert_eq!(
            spec.sources()
                .map(|source| (source.request_url().to_owned(), source.redirects))
                .collect::<Vec<_>>(),
            [
                (
                    rootfs.mirror_url().to_owned(),
                    RedirectPolicy::GithubReleaseAsset
                ),
                (rootfs.upstream()[0].to_owned(), RedirectPolicy::None),
            ]
        );
        assert_eq!(spec.size, SizePolicy::Exact(rootfs.size()));
        assert_eq!(spec.integrity, IntegrityPolicy::Sha256(rootfs.sha256()));

        // Only the built-in FEX has other sources; a custom source has none.
        for spec in &catalog.specs {
            let builtin_fex = matches!(
                spec.kind,
                ArtifactKind::FexPackage | ArtifactKind::FexRootFs
            );
            assert_eq!(spec.fallback_urls.is_empty(), !builtin_fex, "{spec:?}");
        }
        let sources = crate::sources::DownloadSources::resolve(
            &[(
                crate::sources::DownloadArtifact::Fex,
                "https://mirror.example/fex.deb".to_owned(),
            )]
            .into(),
            &std::collections::BTreeMap::new(),
        );
        let custom = ArtifactCatalog::with_sources(&sources);
        let custom = custom.spec(ArtifactKind::FexPackage);
        assert!(custom.is_custom());
        assert_eq!(custom.sources().count(), 1);
    }

    #[test]
    fn launchpads_file_link_may_lead_only_to_its_librarians_copy_of_that_file() {
        let spec = ArtifactCatalog::pinned()
            .spec(ArtifactKind::FexPackage)
            .sources()
            .nth(1)
            .unwrap();
        let name = spec.request_url().rsplit_once('/').unwrap().1.to_owned();
        assert!(name.ends_with("_arm64.deb"), "{name}");
        assert!(spec.approves_final_url(spec.request_url()));
        assert!(
            spec.approves_final_url(&format!("https://launchpadlibrarian.net/877920585/{name}"))
        );
        for refused in [
            format!("https://launchpadlibrarian.net/877920585/other_{name}"),
            format!("https://launchpadlibrarian.net/87a/{name}"),
            format!("https://launchpadlibrarian.net//{name}"),
            "https://launchpadlibrarian.net/877920585/".to_owned(),
            format!("https://launchpadlibrarian.net.evil.invalid/877920585/{name}"),
            format!("http://launchpadlibrarian.net/877920585/{name}"),
            format!("https://launchpadlibrarian.net/877920585/x/{name}"),
            format!("https://elsewhere.invalid/{name}"),
        ] {
            assert!(!spec.approves_final_url(&refused), "{refused}");
        }

        // The workflow accepts what came from a fallback, and only from where
        // that fallback may lead.
        let catalog = ArtifactCatalog::pinned();
        let pinned = catalog.spec(ArtifactKind::FexPackage);
        let librarian = format!("https://launchpadlibrarian.net/877920585/{name}");
        let fixture = tempfile::tempdir().unwrap();
        let artifact = |requested: &str, served: &str| {
            AcquiredArtifact::new(fixture.path().join("fex"), requested, served, None)
        };
        assert!(
            validate_artifact_provenance(pinned, &artifact(spec.request_url(), &librarian)).is_ok()
        );
        assert_eq!(
            validate_artifact_provenance(
                pinned,
                &artifact(spec.request_url(), "https://elsewhere.invalid/fex.deb")
            ),
            Err(ArtifactRejection::UnapprovedFinalUrl)
        );
        // A librarian link is not a source of its own.
        assert_eq!(
            validate_artifact_provenance(pinned, &artifact(&librarian, &librarian)),
            Err(ArtifactRejection::RequestUrlMismatch)
        );
    }

    #[test]
    fn pinned_catalog_freezes_reviewed_artifact_contracts() {
        let catalog = ArtifactCatalog::pinned();
        let runtime = catalog.spec(ArtifactKind::ManagedRuntime);
        assert_eq!(runtime.kind(), ArtifactKind::ManagedRuntime);
        let profile = crate::runtime::current();
        assert_eq!(runtime.request_url(), profile.archive_url);
        assert_eq!(runtime.size, SizePolicy::Exact(profile.archive_size));
        assert_eq!(
            runtime.integrity,
            IntegrityPolicy::Sha512(profile.archive_sha512)
        );
        assert_eq!(runtime.redirects, RedirectPolicy::GithubReleaseAsset);
        assert!(matches!(runtime.integrity, IntegrityPolicy::Sha512(_)));
        assert!(runtime.approves_final_url(
            "https://release-assets.githubusercontent.com/github-production-release-asset/file?token=fixture"
        ));
        assert!(
            !runtime.approves_final_url(
                "https://release-assets.githubusercontent.com.evil.invalid/file"
            )
        );

        // No built-in source is cleartext.
        for source in catalog.specs.iter().flat_map(ArtifactSpec::sources) {
            assert!(source.request_url().starts_with("https://"), "{source:?}");
        }

        let base = catalog.spec(ArtifactKind::BaseGameWrapper);
        assert_eq!(base.expected_filename, Some("Downloader_Diablo2_enUS.exe"));
        assert_eq!(base.integrity, IntegrityPolicy::PeExecutable);

        let mono = catalog.spec(ArtifactKind::WineMono);
        assert_eq!(
            mono.request_url(),
            "https://dl.winehq.org/wine/wine-mono/11.3.0/wine-mono-11.3.0-x86.msi"
        );
        assert_eq!(mono.redirects, RedirectPolicy::None);
        assert_eq!(mono.size, SizePolicy::Exact(83_348_992));
        assert_eq!(
            mono.integrity,
            IntegrityPolicy::Sha256(
                "df2dfc1665c2511882e7cabd56eafd0c0a3d94e5a7e86f969277f6c189d418d3"
            )
        );

        let dotnet = catalog.spec(ArtifactKind::DotnetDesktop);
        assert_eq!(
            dotnet.request_url(),
            "https://builds.dotnet.microsoft.com/dotnet/WindowsDesktop/6.0.36/windowsdesktop-runtime-6.0.36-win-x86.exe"
        );
        assert_eq!(dotnet.redirects, RedirectPolicy::None);
        assert_eq!(dotnet.size, SizePolicy::Exact(52_173_224));
        assert_eq!(
            dotnet.integrity,
            IntegrityPolicy::Sha512(
                "a18351aabfe1590e58af79e57ac2414254ba80cb7a1fef19545a6b8418575c735fc1dc164c3c7fed426c4698f099991487fa4f443bab93afd41d1563845fbcf4"
            )
        );
    }

    #[test]
    fn a_custom_source_replaces_only_its_entry_and_never_relaxes_builtins() {
        use crate::sources::{DownloadArtifact, DownloadSources};
        let builtin = ArtifactCatalog::pinned();
        assert_eq!(
            ArtifactCatalog::with_sources(&DownloadSources::builtin()).specs,
            builtin.specs
        );
        let sources = DownloadSources::resolve(
            &[(
                DownloadArtifact::D2Installer,
                "https://mirror.example/d2.exe".to_owned(),
            )]
            .into(),
            &[
                (
                    DownloadArtifact::WineMono,
                    "http://mirror.example/mono.msi".to_owned(),
                ),
                (
                    DownloadArtifact::DotnetDesktop,
                    "https://mirror.example/dotnet.exe".to_owned(),
                ),
            ]
            .into(),
        );
        let catalog = ArtifactCatalog::with_sources(&sources);
        for (kind, url) in [
            (ArtifactKind::WineMono, "http://mirror.example/mono.msi"),
            (
                ArtifactKind::DotnetDesktop,
                "https://mirror.example/dotnet.exe",
            ),
        ] {
            let spec = catalog.spec(kind);
            assert!(spec.is_custom());
            assert_eq!(spec.request_url(), url);
            assert_eq!(spec.integrity, IntegrityPolicy::Unverified);
        }
        for spec in &catalog.specs {
            if matches!(
                spec.kind,
                ArtifactKind::WineMono | ArtifactKind::DotnetDesktop
            ) {
                continue;
            }
            if spec.kind == ArtifactKind::BaseGameWrapper {
                assert!(spec.is_custom());
                assert_eq!(spec.request_url(), "https://mirror.example/d2.exe");
                assert_eq!(spec.expected_filename, None);
                assert_eq!(spec.redirects, RedirectPolicy::AnyHttp);
                assert_eq!(
                    spec.maximum_bytes(),
                    crate::sources::CUSTOM_ARTIFACT_MAX_BYTES
                );
            } else {
                assert_eq!(spec, builtin.spec(spec.kind));
                assert!(!spec.is_custom());
            }
        }
    }

    #[test]
    fn a_second_mutator_cannot_own_the_managed_installation() {
        let fixture = TempDir::new().expect("fixture");
        let lock_path = fixture.path().join(".pd2launcher.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .expect("lock file");
        File::try_lock(&lock).expect("fixture owns exclusive lease");
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Satisfied,
        )]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        assert_eq!(
            installer.run(fixture.path(), None),
            Err(InstallError::InstallationBusy)
        );
    }

    #[test]
    fn artifact_cache_cannot_escape_through_a_symlink() {
        let fixture = TempDir::new().expect("fixture");
        let outside = TempDir::new().expect("outside fixture");
        std::os::unix::fs::symlink(outside.path(), fixture.path().join("cache"))
            .expect("cache symlink");
        let auditor = ScriptedAuditor::new([audit(
            InstallationStatus::NeedsBaseGame,
            CheckStatus::Missing,
        )]);
        let source = FixtureArtifactSource::default();
        let driver = RecordingDriver::default();
        let client = RecordingClientSynchronizer::default();
        let mut installer =
            ManagedInstaller::new(&auditor, &source, &driver, &client, fixture_catalog());

        // The Download result lives in the cache too: nothing is read or
        // written through the link.
        assert_eq!(
            installer.run(fixture.path(), None),
            Err(InstallError::StageFailed {
                stage: InstallStage::Download,
            })
        );
        assert_eq!(
            outside
                .path()
                .read_dir()
                .expect("outside remains readable")
                .count(),
            0
        );
    }

    fn audit(status: InstallationStatus, runtime: CheckStatus) -> InstallAudit {
        InstallAudit::new(status, HostSupport::Supported, runtime, runtime)
    }

    /// The audits of a fresh install through all three steps: Download
    /// (which sets up Wine for Blizzard's downloader), Install Diablo II,
    /// and Install Project Diablo 2.
    fn fresh_journey() -> [InstallAudit; 7] {
        [
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Missing),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsBaseGame, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsExpansion, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            audit(InstallationStatus::NeedsClient, CheckStatus::Satisfied),
            audit(InstallationStatus::Ready, CheckStatus::Satisfied),
        ]
    }

    struct ScriptedAuditor {
        reports: RefCell<VecDeque<InstallAudit>>,
    }

    impl ScriptedAuditor {
        fn new(reports: impl IntoIterator<Item = InstallAudit>) -> Self {
            Self {
                reports: RefCell::new(reports.into_iter().collect()),
            }
        }

        fn remaining(&self) -> usize {
            self.reports.borrow().len()
        }
    }

    impl InstallationAuditor for ScriptedAuditor {
        fn audit(&self, _paths: &ManagedPaths) -> Result<InstallAudit, AdapterFailure> {
            self.reports
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| AdapterFailure::here())
        }
    }

    #[derive(Default)]
    struct FixtureArtifactSource {
        acquisitions: RefCell<Vec<ArtifactKind>>,
    }

    impl ArtifactSource for FixtureArtifactSource {
        fn acquire_at(
            &self,
            spec: &ArtifactSpec,
            cache: &Path,
            _cache_directory: &Directory,
            _control: &OperationControl,
        ) -> Result<AcquiredArtifact, AdapterFailure> {
            self.acquisitions.borrow_mut().push(spec.kind);
            fs::create_dir_all(cache).map_err(|_| AdapterFailure::here())?;
            let path = cache.join(format!("{:?}.artifact", spec.kind));
            let bytes = if spec.expected_filename.is_some() {
                minimal_pe()
            } else {
                b"fixture".to_vec()
            };
            assert!(bytes.len() as u64 <= spec.maximum_bytes());
            assert!(spec.approves_final_url(spec.request_url()));
            fs::write(&path, &bytes).map_err(|_| AdapterFailure::here())?;
            let acquired = AcquiredArtifact::new(
                path,
                spec.request_url(),
                spec.request_url(),
                spec.expected_filename.map(str::to_owned),
            );
            Ok(if spec.expected_filename.is_some() {
                acquired.with_retained_receipt_sha256(hex(&Sha256::digest(&bytes)))
            } else {
                acquired
            })
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RecordedOperation {
        ManagedRuntime,
        BaseGameDownload,
        ExpansionDownload,
        BaseGame,
        Expansion,
        Import(ImportMode),
        ClientBootstrap,
    }

    #[derive(Default)]
    struct RecordingDriver {
        operations: RefCell<Vec<RecordedOperation>>,
        received_base_secret: RefCell<Option<bool>>,
        received_expansion_secret: RefCell<Option<bool>>,
        markers: RefCell<Vec<crate::stage_marker::MarkerState>>,
        fail_expansion: bool,
        /// The failure a failing expansion run reports.
        expansion_failure: Option<VendorFailure>,
        /// How many base-game downloads fail in the downloader wrapper.
        wrapper_download_failures: std::cell::Cell<usize>,
        /// What those failures are; WrapperDownload when none is set.
        wrapper_download_failure: std::cell::Cell<Option<VendorFailure>>,
        wrapper_download_stalls: std::cell::Cell<usize>,
        fail_import: bool,
    }

    impl InstallDriver for RecordingDriver {
        fn provision_runtime(
            &self,
            paths: &ManagedPaths,
            wine: Option<&VerifiedArtifact>,
            prefix: PrefixArtifacts<'_>,
            _mutation_lease: &File,
            _control: &OperationControl,
        ) -> Result<(), AdapterFailure> {
            if let Some(wine) = wine {
                assert_eq!(wine.kind(), ArtifactKind::ManagedRuntime);
                assert!(wine.path().is_file());
                assert_eq!(wine.observed_sha256(), FIXTURE_SHA256);
                assert_eq!(wine.requested_url(), wine.final_url());
            }
            assert!(
                prefix
                    .gecko
                    .is_none_or(|gecko| gecko.kind() == ArtifactKind::Gecko)
            );
            assert!(
                prefix
                    .mono
                    .is_none_or(|mono| mono.kind() == ArtifactKind::WineMono)
            );
            assert!(
                prefix
                    .dotnet
                    .is_none_or(|dotnet| dotnet.kind() == ArtifactKind::DotnetDesktop)
            );
            assert!(paths.runtime().starts_with(paths.root()));
            assert!(paths.prefix().starts_with(paths.root()));
            assert!(paths.game().starts_with(paths.prefix()));
            self.markers
                .borrow_mut()
                .push(crate::stage_marker::read(paths.directory()));
            self.operations
                .borrow_mut()
                .push(RecordedOperation::ManagedRuntime);
            Ok(())
        }

        fn download_vendor_payload(
            &self,
            paths: &ManagedPaths,
            wrapper: &VerifiedArtifact,
            _mutation_lease: &File,
            _control: &OperationControl,
        ) -> Result<(), VendorFailure> {
            assert!(wrapper.path().is_file());
            self.markers
                .borrow_mut()
                .push(crate::stage_marker::read(paths.directory()));
            if wrapper.kind() == ArtifactKind::BaseGameWrapper
                && self.wrapper_download_failures.get() > 0
            {
                self.wrapper_download_failures
                    .set(self.wrapper_download_failures.get() - 1);
                return Err(self
                    .wrapper_download_failure
                    .get()
                    .unwrap_or(VendorFailure::WrapperDownload));
            }
            if wrapper.kind() == ArtifactKind::BaseGameWrapper
                && self.wrapper_download_stalls.get() > 0
            {
                self.wrapper_download_stalls
                    .set(self.wrapper_download_stalls.get() - 1);
                return Err(VendorFailure::DownloadStalled);
            }
            // The verified payload lands in the product's cache folder.
            let directory = match wrapper.kind() {
                ArtifactKind::BaseGameWrapper => "d2-payload",
                _ => "lod-payload",
            };
            let payload = paths.root().join("cache/artifacts").join(directory);
            fs::create_dir_all(&payload).map_err(|_| VendorFailure::Install)?;
            fs::write(payload.join("Installer.exe"), minimal_pe())
                .map_err(|_| VendorFailure::Install)?;
            self.operations.borrow_mut().push(match wrapper.kind() {
                ArtifactKind::BaseGameWrapper => RecordedOperation::BaseGameDownload,
                _ => RecordedOperation::ExpansionDownload,
            });
            Ok(())
        }

        fn import_existing(
            &self,
            paths: &ManagedPaths,
            _existing: &ExistingInstall,
            mode: ImportMode,
            _mutation_lease: &File,
            _control: &OperationControl,
        ) -> Result<(), AdapterFailure> {
            self.markers
                .borrow_mut()
                .push(crate::stage_marker::read(paths.directory()));
            if self.fail_import {
                return Err(AdapterFailure::here());
            }
            self.operations
                .borrow_mut()
                .push(RecordedOperation::Import(mode));
            Ok(())
        }

        fn install_vendor(
            &self,
            request: VendorInstallRequest<'_>,
            _mutation_lease: &File,
            _control: &OperationControl,
        ) -> Result<(), VendorFailure> {
            assert!(request.artifact().path().is_file());
            assert!(request.paths().game().starts_with(request.paths().prefix()));
            let was_present = request.with_product_key(|key| !key.is_empty());
            self.markers
                .borrow_mut()
                .push(crate::stage_marker::read(request.paths().directory()));
            if self.fail_expansion && request.product() == VendorProduct::Expansion {
                return Err(self.expansion_failure.unwrap_or(VendorFailure::Install));
            }
            match request.product() {
                VendorProduct::BaseGame => {
                    *self.received_base_secret.borrow_mut() = Some(was_present);
                    self.operations
                        .borrow_mut()
                        .push(RecordedOperation::BaseGame);
                }
                VendorProduct::Expansion => {
                    *self.received_expansion_secret.borrow_mut() = Some(was_present);
                    self.operations
                        .borrow_mut()
                        .push(RecordedOperation::Expansion);
                }
            }
            Ok(())
        }

        fn install_client_bootstrap(
            &self,
            _paths: &ManagedPaths,
            _bootstrap: &VerifiedArtifact,
            _mutation_lease: &File,
            _control: &OperationControl,
        ) -> Result<(), AdapterFailure> {
            self.operations
                .borrow_mut()
                .push(RecordedOperation::ClientBootstrap);
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingClientSynchronizer {
        calls: RefCell<usize>,
        prefetches: RefCell<usize>,
    }

    impl ClientSynchronizer for RecordingClientSynchronizer {
        fn prefetch(
            &self,
            paths: &ManagedPaths,
            _control: &OperationControl,
        ) -> Result<(), AdapterFailure> {
            *self.prefetches.borrow_mut() += 1;
            let cache = paths.root().join(CLIENT_PREFETCH);
            fs::create_dir_all(&cache).map_err(|_| AdapterFailure::here())?;
            fs::write(cache.join("manifest.json"), b"{}").map_err(|_| AdapterFailure::here())
        }

        fn synchronize(
            &self,
            _paths: &ManagedPaths,
            _control: &OperationControl,
        ) -> Result<(), AdapterFailure> {
            *self.calls.borrow_mut() += 1;
            Ok(())
        }
    }

    fn fixture_catalog() -> ArtifactCatalog {
        ArtifactCatalog::new([
            fixed_spec(ArtifactKind::ManagedRuntime),
            fixed_spec(ArtifactKind::Gecko),
            fixed_spec(ArtifactKind::WineMono),
            fixed_spec(ArtifactKind::DotnetDesktop),
            wrapper_spec(ArtifactKind::BaseGameWrapper),
            wrapper_spec(ArtifactKind::ExpansionWrapper),
            fixed_spec(ArtifactKind::ClientBootstrap),
            fixed_spec(ArtifactKind::FexPackage),
            fixed_spec(ArtifactKind::FexRootFs),
        ])
    }

    fn fixed_spec(kind: ArtifactKind) -> ArtifactSpec {
        ArtifactSpec {
            kind,
            request_url: Cow::Borrowed(fixture_url(kind)),
            redirects: RedirectPolicy::None,
            size: SizePolicy::Exact(7),
            integrity: IntegrityPolicy::Sha256(FIXTURE_SHA256),
            expected_filename: None,
            fallback_urls: &[],
        }
    }

    fn wrapper_spec(kind: ArtifactKind) -> ArtifactSpec {
        let filename = match kind {
            ArtifactKind::BaseGameWrapper => "Downloader_Diablo2_enUS.exe",
            ArtifactKind::ExpansionWrapper => "Downloader_Diablo2_Lord_of_Destruction_enUS.exe",
            _ => panic!("wrapper kind expected"),
        };
        ArtifactSpec {
            kind,
            request_url: Cow::Borrowed(fixture_url(kind)),
            redirects: RedirectPolicy::None,
            size: SizePolicy::Bounded { min: 64, max: 256 },
            integrity: IntegrityPolicy::PeExecutable,
            expected_filename: Some(filename),
            fallback_urls: &[],
        }
    }

    fn fixture_url(kind: ArtifactKind) -> &'static str {
        match kind {
            ArtifactKind::ManagedRuntime => "https://fixtures.invalid/runtime",
            ArtifactKind::Gecko => "https://fixtures.invalid/gecko",
            ArtifactKind::WineMono => "https://fixtures.invalid/mono",
            ArtifactKind::DotnetDesktop => "https://fixtures.invalid/dotnet",
            ArtifactKind::BaseGameWrapper => "https://fixtures.invalid/d2",
            ArtifactKind::ExpansionWrapper => "https://fixtures.invalid/lod",
            ArtifactKind::ClientBootstrap => "https://fixtures.invalid/bootstrap",
            ArtifactKind::FexPackage => "https://fixtures.invalid/fex.deb",
            ArtifactKind::FexRootFs => "https://fixtures.invalid/rootfs.sqsh",
        }
    }

    fn minimal_pe() -> Vec<u8> {
        let mut bytes = vec![0_u8; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&64_u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes
    }
}
