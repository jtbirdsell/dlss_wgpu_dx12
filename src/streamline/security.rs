//! Authenticode signature verification for `sl.interposer.dll`.
//!
//! The spike intentionally loaded the interposer with **no** signature check. That is unacceptable
//! for an enterprise integration: `sl.interposer.dll` is loaded into our process from a path on disk
//! (a classic DLL-hijack surface). Before we ever `LoadLibrary` it, we hard-gate on two checks:
//!
//!   1. **Trust** — [`WinVerifyTrust`] with `WINTRUST_ACTION_GENERIC_VERIFY_V2` validates the
//!      embedded Authenticode signature and confirms its certificate chain terminates at a trusted
//!      root in the machine/user trust stores (this is the same check Windows applies when you
//!      double-click a signed binary). Unsigned, tampered, expired-chain, or untrusted binaries
//!      return a failure HRESULT and we refuse to load. We additionally request **revocation
//!      checking of the whole chain** (`WTD_REVOKE_WHOLECHAIN` + `WTD_REVOCATION_CHECK_CHAIN`) so a
//!      *revoked* signing certificate is rejected, not merely an untrusted one. Because revocation
//!      requires reaching a CRL/OCSP responder, an *offline* host would otherwise hard-fail a
//!      legitimately-signed binary; we therefore degrade gracefully: if (and only if) the revocation
//!      server is unreachable we retry once with revocation disabled and emit a loud
//!      `REVOCATION-CHECK-SKIPPED` warning for SIEM. A genuinely revoked certificate stays a hard
//!      failure.
//!
//!   2. **Signer identity (fail-closed)** — we then crack the embedded PKCS#7 with
//!      [`CryptQueryObject`], pull the signer's leaf certificate, and require its subject common
//!      name to contain "NVIDIA". This raises the bar from "signed by *someone* Windows trusts" to
//!      "signed by NVIDIA specifically". A *successfully parsed* non-NVIDIA subject is always a hard
//!      [`StreamlineError::UntrustedSigner`]. A query/parse *failure* after trust has already passed
//!      is, **by default, also a hard failure** ([`StreamlineError::SignatureVerificationFailed`]):
//!      if we cannot positively confirm the signer is NVIDIA we refuse to load. Setting
//!      `STREAMLINE_ALLOW_UNVERIFIED_SIGNER=1` opts out of that default and degrades a *parse
//!      failure* (only) back to a logged soft pass that rides on the WinVerifyTrust gate alone; a
//!      parseable non-NVIDIA subject is still rejected regardless. The deprecated
//!      `STREAMLINE_REQUIRE_NVIDIA_SIGNER=1` is now redundant (fail-closed is the default) but is
//!      still honored: it re-asserts the hard gate and overrides a stray
//!      `STREAMLINE_ALLOW_UNVERIFIED_SIGNER=1`.
//!
//! Verification level achieved: full chain-to-trusted-root validation (hard gate) plus signer
//! subject-name pinning to NVIDIA (hard gate; a parse failure is also hard-failed by default, and
//! only soft-passes when explicitly opted out via `STREAMLINE_ALLOW_UNVERIFIED_SIGNER=1`).

use super::types::StreamlineError;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, HWND};
use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_NAME_SIMPLE_DISPLAY_TYPE,
    CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_ENCODING_TYPE,
    CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_FILE, CMSG_SIGNER_INFO,
    CMSG_SIGNER_INFO_PARAM, CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext,
    CertGetNameStringW, CryptMsgClose, CryptMsgGetParam, CryptQueryObject, HCERTSTORE,
    PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_DATA_REVOCATION_CHECKS, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
    WTD_REVOCATION_CHECK_CHAIN, WTD_REVOKE_NONE, WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows::core::PCWSTR;

/// `WinVerifyTrust` returns `0` (`S_OK`) when the file is trusted; any non-zero value is a failure
/// HRESULT (e.g. `TRUST_E_NOSIGNATURE`, `TRUST_E_SUBJECT_NOT_TRUSTED`, `CERT_E_UNTRUSTEDROOT`).
const WIN_VERIFY_TRUST_S_OK: i32 = 0;

/// HRESULTs that indicate revocation could **not be performed** because the revocation server was
/// offline/unreachable (as opposed to the certificate actually being revoked). These are the only
/// failures for which we degrade to a no-revocation retry; every other `TRUST_E_*`/`CERT_E_*` —
/// including an actually-revoked cert (`CERT_E_REVOKED`) — remains a hard failure.
///
/// Stored as `u32` so they compare cleanly against the `WinVerifyTrust` `i32` HRESULT reinterpreted
/// as `u32` (these codes have the high bit set and are negative as `i32`).
///   * `CERT_E_REVOCATION_FAILURE`  (`0x800B010E`) — the revocation function could not check.
///   * `CRYPT_E_REVOCATION_OFFLINE` (`0x80092013`) — the revocation server was offline.
///   * `CRYPT_E_NO_REVOCATION_CHECK`(`0x80092012`) — no revocation check could be performed.
const REVOCATION_OFFLINE_HRESULTS: [u32; 3] = [0x800B_010E, 0x8009_2013, 0x8009_2012];

/// The signer-name substring we pin the interposer to (case-insensitive compare).
const REQUIRED_SIGNER_SUBSTR: &str = "NVIDIA";

/// Environment variable that, when set to `"1"`, opts OUT of the fail-closed default: a signer
/// *parse failure* after the trust gate has already passed degrades to a logged soft pass instead of
/// a hard `Err`. (A parseable, non-NVIDIA subject is rejected regardless.) Off by default, so the
/// default posture is fail-closed: we refuse to load unless the signer is positively confirmed to be
/// NVIDIA. Overridden by [`REQUIRE_NVIDIA_SIGNER_ENV`] when both are set.
const ALLOW_UNVERIFIED_SIGNER_ENV: &str = "STREAMLINE_ALLOW_UNVERIFIED_SIGNER";

/// **Deprecated / redundant.** Historically this var (`="1"`) promoted the signer pin from
/// best-effort to a hard gate. Fail-closed is now the *default*, so this is no longer required. It
/// is still honored for compatibility and as an explicit guard: when set to `"1"` it re-asserts the
/// hard gate and **overrides** a stray [`ALLOW_UNVERIFIED_SIGNER_ENV`]`=1`, so a deployment that
/// hard-pins cannot be silently relaxed by also setting the opt-out.
const REQUIRE_NVIDIA_SIGNER_ENV: &str = "STREAMLINE_REQUIRE_NVIDIA_SIGNER";

/// An open, share-locked handle to a DLL that passed signature verification. Holding this guard
/// alive across [`super::ffi`]'s `Library::new` keeps the file open with `FILE_SHARE_READ` ONLY (no
/// write/delete sharing), so the exact bytes that passed `WinVerifyTrust` are far harder to swap or
/// delete before/while `LoadLibrary` maps them.
///
/// This **narrows** — it does NOT absolutely close — the verify->load TOCTOU (audit L10): per current
/// research ("False File Immutability"), a deny-write share lock is a strong mitigation, not a
/// guarantee of immutability (writable section mappings, network redirectors, transactional tricks
/// can still bypass share modes). The handle is intentionally NOT passed to `Library::new` (which
/// must keep its plain, non-canonical path + default search order so the interposer can find its
/// sibling plugins); it is purely an independent share-lock, closed on drop.
#[derive(Debug)]
pub(crate) struct VerifiedDll {
    handle: HANDLE,
}

impl Drop for VerifiedDll {
    fn drop(&mut self) {
        // SAFETY: `handle` was returned valid by `CreateFileW` (a `VerifiedDll` is only constructed
        // on success) and is closed exactly once here. This is the ONLY `Drop` L10/M8 introduce and
        // it closes the FILE HANDLE — never the leaked interposer `Library` (no `FreeLibrary`).
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Open `path` for read with a deny-write/deny-delete share mode (`FILE_SHARE_READ` only), so that
/// once verified its bytes are far harder to replace/remove while the handle is held. `LoadLibrary`
/// opens with read+execute sharing, so it can still map a file we hold this way.
///
/// **Share-mode note:** if `Library::new` ever fails with `ERROR_SHARING_VIOLATION` against this
/// lock on some loader/filesystem, the ONLY sanctioned relaxation is to additionally OR in
/// `FILE_SHARE_DELETE` (which still blocks the rename-over-write swap) — **never** `FILE_SHARE_WRITE`.
fn open_locked_for_verify(path: &Path, wide_path: &[u16]) -> Result<HANDLE, StreamlineError> {
    // SAFETY: `wide_path` is a NUL-terminated UTF-16 path. We request GENERIC_READ with
    // FILE_SHARE_READ only, OPEN_EXISTING (never create), no security attrs, no template. CreateFileW
    // maps an invalid handle to `Err`, so a returned `Ok` is a live handle owned by us (closed via
    // the `VerifiedDll` guard, or here on the error path).
    unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| {
        StreamlineError::SignatureVerificationFailed(format!(
            "could not open '{}' for verification with a deny-write share lock: {e}",
            path.display()
        ))
    })
}

/// Shared verification core: open `path` with a deny-write share lock, then run BOTH gates
/// (trusted-chain trust + fail-closed NVIDIA signer pin) against that exact handle, returning the
/// still-open guard on success. A caller about to `LoadLibrary` the file holds the guard across the
/// load (see [`verify_interposer_signature`]); a caller that only needs a yes/no answer drops it
/// immediately (see [`verify_signed_dll`]). On any gate failure the guard drops and closes the handle.
fn verify_dll_locked(path: &Path) -> Result<VerifiedDll, StreamlineError> {
    // Encode the path as a NUL-terminated UTF-16 string for the Win32 wide APIs.
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let guard = VerifiedDll {
        handle: open_locked_for_verify(path, &wide_path)?,
    };
    verify_trust(path, &wide_path, guard.handle)?;
    verify_signer_is_nvidia(path, &wide_path)?;
    Ok(guard)
}

/// Verify the interposer (trusted chain + fail-closed NVIDIA signer pin) and RETURN an open,
/// share-locked handle guard. The caller MUST hold the returned [`VerifiedDll`] across
/// `Library::new` so the verified bytes are far harder to swap between verify and load (narrows the
/// verify->load TOCTOU; see [`VerifiedDll`]). Any untrusted / unsigned / tampered binary, or a signer
/// that cannot be confirmed NVIDIA, yields an `Err` and the caller MUST NOT load the DLL.
pub(crate) fn verify_interposer_signature(path: &Path) -> Result<VerifiedDll, StreamlineError> {
    verify_dll_locked(path)
}

/// Verify a sibling SL plugin DLL that the interposer pulls in via the default search order (the M8
/// pre-load pass in [`super::ffi`]). Same trusted-chain + NVIDIA-pin gate as the interposer; returns
/// `Ok(())` because we do not map these ourselves (the share-locked handle is released immediately —
/// there is nothing of ours to pin them against here). A present-but-unverifiable sibling is a hard
/// `Err`.
pub(crate) fn verify_signed_dll(path: &Path) -> Result<(), StreamlineError> {
    verify_dll_locked(path).map(drop)
}

// `OsStr::encode_wide` lives in the Windows-only extension trait.
use std::os::windows::ffi::OsStrExt;

/// Step 1 (hard gate): `WinVerifyTrust` chain-to-trusted-root validation **with revocation
/// checking of the whole chain**.
///
/// We first verify with `WTD_REVOKE_WHOLECHAIN` + `WTD_REVOCATION_CHECK_CHAIN`. If that fails
/// *specifically* because the revocation server was unreachable (see [`REVOCATION_OFFLINE_HRESULTS`])
/// — e.g. an air-gapped or offline host — we retry **once** with revocation disabled and emit a loud
/// `REVOCATION-CHECK-SKIPPED` warning for SIEM. Any other failure (untrusted root, no signature,
/// tampered, or an *actually revoked* certificate) stays a hard `Err`.
fn verify_trust(path: &Path, wide_path: &[u16], handle: HANDLE) -> Result<(), StreamlineError> {
    // Attempt 1: full-chain revocation checking.
    match verify_trust_once(wide_path, handle, WTD_REVOKE_WHOLECHAIN, true) {
        WIN_VERIFY_TRUST_S_OK => {
            log::debug!(
                "WinVerifyTrust: '{}' is Authenticode-signed, chains to a trusted root, and passed \
                 whole-chain revocation checking",
                path.display()
            );
            Ok(())
        }
        status if is_revocation_offline(status) => {
            // The cert is NOT revoked — we simply could not reach a CRL/OCSP responder. Degrade to
            // a no-revocation verify so an offline host can still load a legitimately-signed binary,
            // but make the downgrade conspicuous and machine-greppable for SIEM.
            log::warn!(
                "REVOCATION-CHECK-SKIPPED: WinVerifyTrust whole-chain revocation check for '{}' \
                 could not reach a revocation server (HRESULT {status:#010x}); retrying once WITHOUT \
                 revocation checking. The signature/chain is still validated; only freshness against \
                 revocation lists is skipped.",
                path.display()
            );
            match verify_trust_once(wide_path, handle, WTD_REVOKE_NONE, false) {
                WIN_VERIFY_TRUST_S_OK => {
                    log::warn!(
                        "REVOCATION-CHECK-SKIPPED: '{}' passed WinVerifyTrust with revocation \
                         checking DISABLED (offline fallback). Chain-to-trusted-root is verified; \
                         revocation status is UNKNOWN.",
                        path.display()
                    );
                    Ok(())
                }
                status => Err(StreamlineError::SignatureVerificationFailed(format!(
                    "WinVerifyTrust (offline no-revocation retry) returned HRESULT {status:#010x} for '{}'",
                    path.display()
                ))),
            }
        }
        status => Err(StreamlineError::SignatureVerificationFailed(format!(
            "WinVerifyTrust returned HRESULT {status:#010x} for '{}'",
            path.display()
        ))),
    }
}

/// Returns `true` if `status` is one of the "revocation server unreachable" HRESULTs (as opposed to
/// the certificate actually being revoked). Compares the `i32` HRESULT reinterpreted as `u32`.
fn is_revocation_offline(status: i32) -> bool {
    REVOCATION_OFFLINE_HRESULTS.contains(&(status as u32))
}

/// Perform a single `WinVerifyTrust` VERIFY pass (always paired with the matching `_CLOSE` cleanup)
/// using the given revocation policy, and return the raw HRESULT.
///
/// `revocation_checks` selects `fdwRevocationChecks`; when `check_chain` is `true` the
/// `WTD_REVOCATION_CHECK_CHAIN` provider flag is OR'd into `dwProvFlags` to actually exercise the
/// chain revocation logic.
fn verify_trust_once(
    wide_path: &[u16],
    handle: HANDLE,
    revocation_checks: WINTRUST_DATA_REVOCATION_CHECKS,
    check_chain: bool,
) -> i32 {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    // L10: also hand WinVerifyTrust the already-open, share-locked `hFile` (deny write/delete) so the
    // verified bytes are far harder to swap before the subsequent `Library::new`. `pcwszFilePath`
    // stays set too — both are valid together; per Microsoft, `hFile` is an optional optimization and
    // the path is still used for display/catalog lookup, so the deny-write share LOCK (not the hFile
    // plumbing) is what does the real hardening. See [`VerifiedDll`].
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide_path.as_ptr()),
        hFile: handle,
        ..Default::default()
    };

    let mut trust_data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: revocation_checks,
        dwUnionChoice: WTD_CHOICE_FILE,
        dwStateAction: WTD_STATEACTION_VERIFY,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        ..Default::default()
    };
    if check_chain {
        // Exercise revocation across the whole chain rather than just the leaf.
        trust_data.dwProvFlags |= WTD_REVOCATION_CHECK_CHAIN;
    }

    // SAFETY: `action` and `trust_data` are valid, properly sized, locally-owned Win32 structs;
    // `trust_data.Anonymous.pFile` points at `file_info`, which outlives this call. We pass a null
    // HWND (no UI). `WinVerifyTrust` does not retain any of these pointers past return. We always
    // issue the matching `WTD_STATEACTION_CLOSE` call below to release the state data it allocated,
    // regardless of the verify result.
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut trust_data as *mut WINTRUST_DATA).cast(),
        )
    };

    // Always close the state data WinVerifyTrust allocated, mirroring the pattern Microsoft
    // documents (re-issue WinVerifyTrust with WTD_STATEACTION_CLOSE on the same WINTRUST_DATA).
    trust_data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: same struct as above, now requesting cleanup of `hWVTStateData`. Return value is
    // intentionally ignored — there is nothing actionable on a close failure.
    unsafe {
        let _ = WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut trust_data as *mut WINTRUST_DATA).cast(),
        );
    }

    status
}

/// Step 2 (hard gate, fail-closed): crack the embedded PKCS#7, pull the signer leaf certificate, and
/// require its subject to contain "NVIDIA".
///
/// A successfully parsed non-NVIDIA subject is always a hard [`StreamlineError::UntrustedSigner`].
/// A failure to *parse* the signature after the trust gate already passed is, **by default, also a
/// hard failure** ([`StreamlineError::SignatureVerificationFailed`]): if we cannot positively
/// confirm the signer is NVIDIA we refuse to load (logged with a `SIGNER-PIN-FAILED` SIEM token).
///
/// Setting `STREAMLINE_ALLOW_UNVERIFIED_SIGNER=1` opts out of the fail-closed default and degrades a
/// *parse failure* (only) back to a logged soft pass (`SIGNER-PIN-SKIPPED`) that rides on the
/// WinVerifyTrust gate alone. The deprecated `STREAMLINE_REQUIRE_NVIDIA_SIGNER=1` re-asserts the
/// hard gate and overrides a stray opt-out.
fn verify_signer_is_nvidia(path: &Path, wide_path: &[u16]) -> Result<(), StreamlineError> {
    let allow_unverified = unverified_signer_allowed();
    match extract_signer_subject(wide_path) {
        Ok(subject) => {
            if subject
                .to_ascii_uppercase()
                .contains(REQUIRED_SIGNER_SUBSTR)
            {
                log::info!(
                    "sl.interposer.dll signer subject verified: {subject:?} (contains \"NVIDIA\")"
                );
                Ok(())
            } else {
                // A parseable, non-NVIDIA subject is always a hard failure (independent of the env
                // vars) — see the function doc.
                Err(StreamlineError::UntrustedSigner(subject))
            }
        }
        Err(detail) => {
            // Trust already passed; we just could not parse the subject. Fail closed by default.
            if allow_unverified {
                log::warn!(
                    "SIGNER-PIN-SKIPPED: sl.interposer.dll passed WinVerifyTrust but its signer \
                     subject could not be extracted for NVIDIA pinning ('{}'): {detail}. \
                     {ALLOW_UNVERIFIED_SIGNER_ENV}=1 — proceeding on the trust gate only (UNSAFE: \
                     the signer is NOT confirmed to be NVIDIA).",
                    path.display()
                );
                Ok(())
            } else {
                log::error!(
                    "SIGNER-PIN-FAILED: sl.interposer.dll passed WinVerifyTrust but its signer \
                     subject could not be extracted for NVIDIA pinning ('{}'): {detail}. \
                     Refusing to load (fail-closed default; set {ALLOW_UNVERIFIED_SIGNER_ENV}=1 to \
                     proceed on the trust gate alone).",
                    path.display()
                );
                Err(StreamlineError::SignatureVerificationFailed(format!(
                    "could not confirm the signer is NVIDIA (fail-closed default; set \
                     {ALLOW_UNVERIFIED_SIGNER_ENV}=1 to allow an unverifiable signer): {detail}"
                )))
            }
        }
    }
}

/// Whether a signer that could not be parsed/confirmed as NVIDIA is *allowed* to load (the opt-out
/// from the fail-closed default), per the `STREAMLINE_ALLOW_UNVERIFIED_SIGNER` /
/// `STREAMLINE_REQUIRE_NVIDIA_SIGNER` env vars.
fn unverified_signer_allowed() -> bool {
    unverified_signer_allowed_from(
        std::env::var_os(ALLOW_UNVERIFIED_SIGNER_ENV),
        std::env::var_os(REQUIRE_NVIDIA_SIGNER_ENV),
    )
}

/// Pure form of [`unverified_signer_allowed`]: an unverifiable signer is allowed only when the
/// opt-out `allow` value is exactly `"1"` AND the deprecated `require` guard is NOT exactly `"1"`
/// (an explicit `STREAMLINE_REQUIRE_NVIDIA_SIGNER=1` re-asserts the hard gate and vetoes the
/// opt-out). Split out so the policy is unit-testable without mutating the process environment.
fn unverified_signer_allowed_from(
    allow: Option<std::ffi::OsString>,
    require: Option<std::ffi::OsString>,
) -> bool {
    let allow = allow.is_some_and(|v| v == "1");
    let require = require.is_some_and(|v| v == "1");
    allow && !require
}

/// Crack the embedded PKCS#7 signature on `wide_path` and return the signer leaf certificate's
/// subject display name. Returns `Err(detail)` if any step fails (caller decides severity).
fn extract_signer_subject(wide_path: &[u16]) -> Result<String, String> {
    let encoding = CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0);

    let mut h_store = HCERTSTORE::default();
    let mut h_msg: *mut core::ffi::c_void = core::ptr::null_mut();

    // SAFETY: `wide_path` is a NUL-terminated UTF-16 path. We request the embedded signature store
    // + message handles; the out-params are valid locals. On success both handles are owned by us
    // and released below via CertCloseStore / CryptMsgClose.
    unsafe {
        CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            wide_path.as_ptr().cast(),
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            None,
            None,
            None,
            Some(&mut h_store),
            Some(&mut h_msg),
            None,
        )
        .map_err(|e| format!("CryptQueryObject failed: {e}"))?;
    }

    // RAII-ish cleanup: ensure the store + message handles are released on every return path.
    let result = extract_subject_from_handles(encoding, h_store, h_msg);

    // SAFETY: `h_msg` was produced by CryptQueryObject above (or is null, which CryptMsgClose
    // tolerates). Released exactly once here.
    unsafe {
        let _ = CryptMsgClose(if h_msg.is_null() { None } else { Some(h_msg) });
    }
    // SAFETY: `h_store` was produced by CryptQueryObject above. Released exactly once here.
    unsafe {
        let _ = CertCloseStore(
            if h_store.is_invalid() {
                None
            } else {
                Some(h_store)
            },
            0,
        );
    }

    result
}

/// Inner helper: given the cracked PKCS#7 message + cert store, find the signer's leaf certificate
/// and return its subject display name.
fn extract_subject_from_handles(
    encoding: CERT_QUERY_ENCODING_TYPE,
    h_store: HCERTSTORE,
    h_msg: *mut core::ffi::c_void,
) -> Result<String, String> {
    // 1. Query the size of the CMSG_SIGNER_INFO_PARAM blob.
    let mut signer_info_len: u32 = 0;
    // SAFETY: querying size only (pvData = None). `h_msg` is a valid cracked message handle.
    unsafe {
        CryptMsgGetParam(h_msg, CMSG_SIGNER_INFO_PARAM, 0, None, &mut signer_info_len)
            .map_err(|e| format!("CryptMsgGetParam(size) failed: {e}"))?;
    }
    if (signer_info_len as usize) < size_of::<CMSG_SIGNER_INFO>() {
        return Err(format!(
            "CMSG_SIGNER_INFO blob too small ({signer_info_len} bytes)"
        ));
    }

    // 2. Fetch the CMSG_SIGNER_INFO blob into an aligned buffer.
    let mut buffer = vec![0u8; signer_info_len as usize];
    // SAFETY: `buffer` is `signer_info_len` bytes; we pass its length back so the API will not
    // overrun it. `h_msg` is valid.
    unsafe {
        CryptMsgGetParam(
            h_msg,
            CMSG_SIGNER_INFO_PARAM,
            0,
            Some(buffer.as_mut_ptr().cast()),
            &mut signer_info_len,
        )
        .map_err(|e| format!("CryptMsgGetParam(data) failed: {e}"))?;
    }

    // The signer info's Issuer + SerialNumber identify the signing certificate within the store.
    // SAFETY: `buffer` holds a CMSG_SIGNER_INFO laid out by the crypto API, but the `Vec<u8>` backing
    // is only 1-byte aligned while CMSG_SIGNER_INFO is pointer-aligned — so we COPY the header out
    // with `read_unaligned` rather than forming a reference to a possibly-misaligned address (which
    // is UB even if the later field reads happen to work). The copied Issuer/SerialNumber blobs'
    // `pbData` pointers still point into `buffer`, which outlives the find call below.
    let signer_info =
        unsafe { core::ptr::read_unaligned(buffer.as_ptr() as *const CMSG_SIGNER_INFO) };

    // 3. Build a CERT_INFO carrying only Issuer + SerialNumber, as CERT_FIND_SUBJECT_CERT expects.
    let mut find_info = CERT_INFO {
        Issuer: signer_info.Issuer,
        SerialNumber: signer_info.SerialNumber,
        ..Default::default()
    };

    // SAFETY: `h_store` is the embedded-signature cert store. We pass our locally-owned `find_info`
    // as the find parameter. On success the returned context borrows from `h_store`, which we keep
    // alive until after we read the name. The pointer is freed via CertFreeCertificateContext.
    let cert_ctx: *mut CERT_CONTEXT = unsafe {
        CertFindCertificateInStore(
            h_store,
            encoding,
            0,
            CERT_FIND_SUBJECT_CERT,
            Some((&mut find_info as *mut CERT_INFO).cast()),
            None,
        )
    };
    if cert_ctx.is_null() {
        return Err("CertFindCertificateInStore found no signer certificate".to_string());
    }

    let subject = read_subject_name(cert_ctx);

    // SAFETY: `cert_ctx` was returned by CertFindCertificateInStore; freed exactly once here.
    unsafe {
        let _ = CertFreeCertificateContext(Some(cert_ctx));
    }

    subject
}

/// Read the subject display name (e.g. `"NVIDIA Corporation"`) from a certificate context.
fn read_subject_name(cert_ctx: *const CERT_CONTEXT) -> Result<String, String> {
    // First call (psznamestring = None) returns the required length in chars (incl. NUL).
    // SAFETY: `cert_ctx` is a valid certificate context. Length-query form passes a null buffer.
    let len = unsafe { CertGetNameStringW(cert_ctx, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None) };
    if len <= 1 {
        // 1 == just the NUL terminator (empty name).
        return Err("certificate has an empty subject name".to_string());
    }

    let mut name = vec![0u16; len as usize];
    // SAFETY: `name` has exactly `len` u16 slots, matching the length the API just reported.
    let written = unsafe {
        CertGetNameStringW(
            cert_ctx,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            None,
            Some(&mut name),
        )
    };
    if written == 0 {
        return Err("CertGetNameStringW returned an empty name".to_string());
    }

    // Drop the trailing NUL before decoding.
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..end]))
}

#[cfg(test)]
mod tests {
    //! Headless tests for the signature hard-gate. These run real Win32 `WinVerifyTrust` against
    //! files on disk — no GPU and no Streamline SDK required, so they run on a Windows CI runner.

    use super::{
        StreamlineError, unverified_signer_allowed_from, verify_interposer_signature,
        verify_signed_dll,
    };
    use std::ffi::OsString;
    use std::io::Write;

    #[test]
    fn unverified_signer_disallowed_by_default_and_allowed_only_on_explicit_opt_out() {
        let one = || Some(OsString::from("1"));
        // Fail-closed by default: nothing set => not allowed.
        assert!(!unverified_signer_allowed_from(None, None));
        // Opt-out alone, exactly "1" => allowed.
        assert!(unverified_signer_allowed_from(one(), None));
        // Opt-out must be exactly "1".
        assert!(!unverified_signer_allowed_from(
            Some(OsString::from("0")),
            None
        ));
        assert!(!unverified_signer_allowed_from(
            Some(OsString::from("true")),
            None
        ));
        assert!(!unverified_signer_allowed_from(
            Some(OsString::from("")),
            None
        ));
        // Deprecated REQUIRE=1 re-asserts the hard gate and vetoes a stray opt-out.
        assert!(!unverified_signer_allowed_from(one(), one()));
        // A non-"1" REQUIRE value does not veto the opt-out.
        assert!(unverified_signer_allowed_from(
            one(),
            Some(OsString::from("0"))
        ));
    }

    #[test]
    fn unsigned_file_is_rejected_by_the_trust_gate() {
        // The hard gate must refuse a file with no valid embedded Authenticode signature. Write a
        // junk (non-PE, unsigned) file and confirm WinVerifyTrust fails it.
        let path = std::env::temp_dir().join("dlss_wgpu_dx12_unsigned_interposer_test.bin");
        {
            let mut f = std::fs::File::create(&path).expect("create temp test file");
            f.write_all(b"not a signed PE -- just junk bytes for the trust gate test")
                .expect("write temp test file");
        }
        let result = verify_interposer_signature(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(result, Err(StreamlineError::SignatureVerificationFailed(_))),
            "expected SignatureVerificationFailed for an unsigned file, got {result:?}"
        );
    }

    #[test]
    fn verify_signed_dll_rejects_unsigned_file() {
        // The path-generic helper used for the sibling SL plugins (M8) must hard-gate an unsigned
        // file exactly like the interposer wrapper — same trust gate, same fail-closed default. This
        // also gives the new open_locked_for_verify path positive coverage on GPU-less CI: the temp
        // file exists and is writable, so CreateFileW with FILE_SHARE_READ succeeds, and the trust
        // gate then rejects the junk bytes.
        let path = std::env::temp_dir().join("dlss_wgpu_dx12_unsigned_sibling_test.bin");
        {
            let mut f = std::fs::File::create(&path).expect("create temp test file");
            f.write_all(b"not a signed PE -- junk bytes for the sibling trust gate test")
                .expect("write temp test file");
        }
        let result = verify_signed_dll(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(result, Err(StreamlineError::SignatureVerificationFailed(_))),
            "expected SignatureVerificationFailed for an unsigned sibling, got {result:?}"
        );
    }

    #[test]
    fn trusted_but_non_nvidia_signer_is_rejected() {
        // Opportunistic + skip-safe: most Windows system files are *catalog*-signed (which the
        // file-based trust gate treats as unsigned), but the Microsoft VC++ runtime DLLs typically
        // carry an *embedded* Authenticode signature. If we can find one that passes the trust gate,
        // its non-NVIDIA signer must trip the pin (`UntrustedSigner`). If none on this runner are
        // embedded-signed (so they all fail the trust gate instead), skip rather than fail.
        let candidates = [
            r"C:\Windows\System32\msvcp140.dll",
            r"C:\Windows\System32\vcruntime140.dll",
            r"C:\Windows\System32\vcruntime140_1.dll",
            r"C:\Windows\System32\concrt140.dll",
            r"C:\Windows\System32\msvcp140_1.dll",
        ];
        for candidate in candidates {
            let path = std::path::Path::new(candidate);
            if !path.exists() {
                continue;
            }
            match verify_interposer_signature(path) {
                Err(StreamlineError::UntrustedSigner(subject)) => {
                    assert!(
                        !subject.to_ascii_uppercase().contains("NVIDIA"),
                        "an NVIDIA subject should not have tripped UntrustedSigner: {subject:?}"
                    );
                    eprintln!(
                        "UntrustedSigner fired for embedded-signed {candidate} (subject {subject:?})"
                    );
                    return;
                }
                // Catalog-signed (treated as unsigned), or signer unparseable (soft pass) — try the
                // next candidate.
                _ => continue,
            }
        }
        eprintln!(
            "skipping: no embedded-signed, non-NVIDIA system binary found to exercise the signer pin"
        );
    }
}
