//! Authenticode signature verification for `sl.interposer.dll`.
//!
//! The spike intentionally loaded the interposer with **no** signature check. That is unacceptable
//! for an enterprise integration: `sl.interposer.dll` is loaded into our process and is also copied
//! next to the host executable as `dxgi.dll`/`d3d12.dll` (a classic DLL-hijack surface). Before we
//! ever `LoadLibrary` it, we hard-gate on two checks:
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
//!   2. **Signer identity (best-effort)** — we then crack the embedded PKCS#7 with
//!      [`CryptQueryObject`], pull the signer's leaf certificate, and require its subject common
//!      name to contain "NVIDIA". This raises the bar from "signed by *someone* Windows trusts" to
//!      "signed by NVIDIA specifically". It is best-effort: a query/parse failure after trust has
//!      already passed is logged and treated as a soft pass (the WinVerifyTrust gate above is the
//!      hard requirement), but a *successfully parsed* non-NVIDIA subject is a hard
//!      [`StreamlineError::UntrustedSigner`].
//!
//! Verification level achieved: full chain-to-trusted-root validation (hard gate) plus signer
//! subject-name pinning to NVIDIA (hard gate when the subject is parseable; soft when it is not).

use super::types::StreamlineError;
use std::path::Path;

use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_NAME_SIMPLE_DISPLAY_TYPE,
    CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_ENCODING_TYPE,
    CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_FILE, CMSG_SIGNER_INFO, CMSG_SIGNER_INFO_PARAM,
    CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext, CertGetNameStringW,
    CryptMsgClose, CryptMsgGetParam, CryptQueryObject, HCERTSTORE, PKCS_7_ASN_ENCODING,
    X509_ASN_ENCODING,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_DATA_REVOCATION_CHECKS, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
    WTD_REVOCATION_CHECK_CHAIN, WTD_REVOKE_NONE, WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
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

/// Environment variable that, when set to `"1"`, promotes the NVIDIA signer pin from best-effort to
/// a hard gate: a parse failure (soft-pass) or a non-NVIDIA subject becomes an `Err` instead of a
/// warning. For high-assurance deployments that refuse to run an unverifiable signer.
const REQUIRE_NVIDIA_SIGNER_ENV: &str = "STREAMLINE_REQUIRE_NVIDIA_SIGNER";

/// Verify that `path` is an Authenticode-signed binary whose certificate chain terminates at a
/// trusted root, and (best-effort) that the signer subject contains "NVIDIA".
///
/// Returns `Ok(())` only if the trust gate passes. Any untrusted / unsigned / tampered binary, or
/// a parseable signer subject that is not NVIDIA, yields an `Err` and the caller MUST NOT load the
/// DLL.
pub(crate) fn verify_interposer_signature(path: &Path) -> Result<(), StreamlineError> {
    // Encode the path as a NUL-terminated UTF-16 string for the Win32 wide APIs.
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    verify_trust(path, &wide_path)?;
    verify_signer_is_nvidia(path, &wide_path)?;
    Ok(())
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
fn verify_trust(path: &Path, wide_path: &[u16]) -> Result<(), StreamlineError> {
    // Attempt 1: full-chain revocation checking.
    match verify_trust_once(wide_path, WTD_REVOKE_WHOLECHAIN, true) {
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
            match verify_trust_once(wide_path, WTD_REVOKE_NONE, false) {
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
    revocation_checks: WINTRUST_DATA_REVOCATION_CHECKS,
    check_chain: bool,
) -> i32 {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide_path.as_ptr()),
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

/// Step 2 (best-effort hard gate): crack the embedded PKCS#7, pull the signer leaf certificate, and
/// require its subject to contain "NVIDIA".
///
/// A failure to *parse* the signature after the trust gate already passed is logged loudly (with a
/// `SIGNER-PIN-SKIPPED` SIEM token) and treated as a soft pass — trust is the load-bearing gate. A
/// successfully parsed non-NVIDIA subject is always a hard [`StreamlineError::UntrustedSigner`].
///
/// When the `STREAMLINE_REQUIRE_NVIDIA_SIGNER` environment variable is set to `"1"`, the soft pass
/// is **promoted to a hard failure** ([`StreamlineError::SignatureVerificationFailed`]): a
/// high-assurance deployment refuses to load an interposer whose signer it cannot positively
/// confirm to be NVIDIA.
fn verify_signer_is_nvidia(path: &Path, wide_path: &[u16]) -> Result<(), StreamlineError> {
    let require_nvidia = signer_pin_is_required();
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
                // var) — see the function doc.
                Err(StreamlineError::UntrustedSigner(subject))
            }
        }
        Err(detail) => {
            // Trust already passed; we just could not parse the subject.
            if require_nvidia {
                log::error!(
                    "SIGNER-PIN-SKIPPED: sl.interposer.dll passed WinVerifyTrust but its signer \
                     subject could not be extracted for NVIDIA pinning ('{}'): {detail}. \
                     {REQUIRE_NVIDIA_SIGNER_ENV}=1 — refusing to load.",
                    path.display()
                );
                Err(StreamlineError::SignatureVerificationFailed(format!(
                    "could not confirm the signer is NVIDIA ({REQUIRE_NVIDIA_SIGNER_ENV}=1 requires \
                     a positively-parsed NVIDIA signer): {detail}"
                )))
            } else {
                log::warn!(
                    "SIGNER-PIN-SKIPPED: sl.interposer.dll passed WinVerifyTrust but its signer \
                     subject could not be extracted for NVIDIA pinning ('{}'): {detail}. \
                     Proceeding on the trust gate only (set {REQUIRE_NVIDIA_SIGNER_ENV}=1 to make \
                     this a hard failure).",
                    path.display()
                );
                Ok(())
            }
        }
    }
}

/// Whether the NVIDIA signer pin is configured as a hard requirement via
/// `STREAMLINE_REQUIRE_NVIDIA_SIGNER=1`.
fn signer_pin_is_required() -> bool {
    signer_pin_required_from(std::env::var_os(REQUIRE_NVIDIA_SIGNER_ENV))
}

/// Pure form of [`signer_pin_is_required`]: the pin is promoted to a hard requirement only when the
/// env value is exactly `"1"`. Split out so the policy is unit-testable without mutating the
/// process environment.
fn signer_pin_required_from(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|v| v == "1")
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
        let _ = CertCloseStore(if h_store.is_invalid() { None } else { Some(h_store) }, 0);
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
    // SAFETY: `buffer` holds a valid CMSG_SIGNER_INFO laid out by the crypto API; reading the
    // header fields by reference does not outlive `buffer`.
    let signer_info = unsafe { &*(buffer.as_ptr() as *const CMSG_SIGNER_INFO) };

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
    let len = unsafe {
        CertGetNameStringW(
            cert_ctx,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            None,
            None,
        )
    };
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

    use super::{signer_pin_required_from, verify_interposer_signature, StreamlineError};
    use std::ffi::OsString;
    use std::io::Write;

    #[test]
    fn signer_pin_required_only_when_env_is_exactly_one() {
        assert!(signer_pin_required_from(Some(OsString::from("1"))));
        assert!(!signer_pin_required_from(Some(OsString::from("0"))));
        assert!(!signer_pin_required_from(Some(OsString::from("true"))));
        assert!(!signer_pin_required_from(Some(OsString::from(""))));
        assert!(!signer_pin_required_from(None));
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
