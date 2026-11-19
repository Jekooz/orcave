//! Orcave Library - SMB network share support with authentication boundaries.
//!
//! SMB shares are reached through UNC paths (e.g. `\\server\share\music`).
//! Credentials are managed exclusively by Windows Credential Manager via
//! `orcave-windows`; the library stores only a credential *target name* and
//! never a password, and no credential ever crosses the Tauri IPC boundary.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A resolved SMB share reference. No secrets inside.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbShare {
    /// UNC path to the share, e.g. `\\server\share`.
    pub unc_path: String,
    /// Credential Manager target name used to authenticate. None means
    /// use the session's default / already-authenticated network context.
    pub credential_target: Option<String>,
    /// Whether the share was verified reachable at last scan.
    pub reachable: bool,
}

/// Result of validating an SMB folder path without exposing credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbValidation {
    pub unc_path: String,
    pub accessible: bool,
    /// Redacted reason when inaccessible (no usernames/passwords).
    pub reason: Option<String>,
}

/// Validate that an SMB UNC path is reachable from this machine.
///
/// Read-only: attempts a directory listing only. Never sends credentials.
pub fn validate_smb_share(unc_path: &str) -> SmbValidation {
    let path = PathBuf::from(unc_path);
    let accessible = std::fs::read_dir(&path).is_ok();
    SmbValidation {
        unc_path: unc_path.to_string(),
        accessible,
        reason: if accessible {
            None
        } else {
            Some("share not reachable or not authorized".to_string())
        },
    }
}

/// Convert a LibraryFolder SMB source into an SmbShare descriptor.
pub fn smb_from_folder(unc_path: &str, credential_target: Option<String>) -> SmbShare {
    SmbShare {
        unc_path: unc_path.to_string(),
        credential_target,
        reachable: false,
    }
}

/// Marker: SMB protocol client is not yet wired. Full SMB credential
/// negotiation (username/password via Credential Manager UI) requires a
/// dedicated SMB client crate. Until then, UNC paths already authenticated
/// by Windows (via Credential Manager / domain logon) are supported.
pub const SMB_CLIENT_PENDING: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smb_validation_redacts_no_credentials() {
        let v = validate_smb_share("\\\\nonexistent\\share");
        assert!(!v.accessible);
        assert!(v.reason.is_some());
        // Reason must not look like a username or password.
        let r = v.reason.unwrap();
        assert!(!r.to_lowercase().contains("password"));
        assert!(!r.to_lowercase().contains("username"));
    }

    #[test]
    fn smb_share_serialization_excludes_secrets() {
        let share = SmbShare {
            unc_path: r"\\server\music".to_string(),
            credential_target: Some("OrcaveSmbServer".to_string()),
            reachable: true,
        };
        let json = serde_json::to_string(&share).unwrap();
        // JSON escapes backslashes: \\server\music -> \\\\server\\music
        assert!(json.contains("\\\\\\\\server\\\\music"), "json: {json}");
        assert!(json.contains("OrcaveSmbServer"));
        assert!(!json.contains("password"));
        assert!(!json.contains("secret"));
    }
}
