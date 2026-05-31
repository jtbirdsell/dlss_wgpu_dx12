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
//!      return a failure HRESULT and we refuse to load.
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
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    WinVerifyTrust,
};
use windows::core::PCWSTR;

/// `WinVerifyTrust` returns `0` (`S_OK`) when the file is trusted; any non-zero value is a failure
/// HRESULT (e.g. `TRUST_E_NOSIGNATURE`, `TRUST_E_SUBJECT_NOT_TRUSTED`, `CERT_E_UNTRUSTEDROOT`).
const WIN_VERIFY_TRUST_S_OK: i32 = 0;

/// The signer-name substring we pin the interposer to (case-insensitive compare).
const REQUIRED_SIGNER_SUBSTR: &str = "NVIDIA";

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

/// Step 1 (hard gate): `WinVerifyTrust` chain-to-trusted-root validation.
fn verify_trust(path: &Path, wide_path: &[u16]) -> Result<(), StreamlineError> {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide_path.as_ptr()),
        ..Default::default()
    };

    let mut trust_data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        dwStateAction: WTD_STATEACTION_VERIFY,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        ..Default::default()
    };

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

    if status == WIN_VERIFY_TRUST_S_OK {
        log::debug!(
            "WinVerifyTrust: '{}' is Authenticode-signed and chains to a trusted root",
            path.display()
        );
        Ok(())
    } else {
        Err(StreamlineError::SignatureVerificationFailed(format!(
            "WinVerifyTrust returned HRESULT {status:#010x} for '{}'",
            path.display()
        )))
    }
}

/// Step 2 (best-effort hard gate): crack the embedded PKCS#7, pull the signer leaf certificate, and
/// require its subject to contain "NVIDIA".
///
/// A failure to *parse* the signature after the trust gate already passed is logged and treated as
/// a soft pass (trust is the load-bearing gate). A successfully parsed non-NVIDIA subject is a hard
/// [`StreamlineError::UntrustedSigner`].
fn verify_signer_is_nvidia(path: &Path, wide_path: &[u16]) -> Result<(), StreamlineError> {
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
                Err(StreamlineError::UntrustedSigner(subject))
            }
        }
        Err(detail) => {
            // Trust already passed; we just could not parse the subject. Do not hard-fail.
            log::warn!(
                "sl.interposer.dll passed WinVerifyTrust but its signer subject could not be \
                 extracted for NVIDIA pinning ('{}'): {detail}. Proceeding on the trust gate only.",
                path.display()
            );
            Ok(())
        }
    }
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
