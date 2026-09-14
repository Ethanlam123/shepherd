//! Launch-at-login through SMAppService (macOS 13+), the modern replacement
//! for SMLoginItemSetEnabled. Registering requires a code-signed .app bundle
//! (ad-hoc from `tauri build` qualifies), so toggling from `tauri dev`
//! returns an error the panel shows instead of silently failing.

use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// True when the main app is registered as a login item.
pub fn is_enabled() -> Result<bool, String> {
    let service = unsafe { SMAppService::mainAppService() };
    Ok(unsafe { service.status() } == SMAppServiceStatus::Enabled)
}

pub fn set(enabled: bool) -> Result<(), String> {
    let service = unsafe { SMAppService::mainAppService() };
    // register on an already-registered service fails with
    // kSMErrorAlreadyRegistered, so skip when the state already matches.
    let registered = unsafe { service.status() } == SMAppServiceStatus::Enabled;
    match (enabled, registered) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => {
            unsafe { service.registerAndReturnError() }.map_err(|e| format!("login item: {e}"))
        }
        (false, true) => {
            unsafe { service.unregisterAndReturnError() }.map_err(|e| format!("login item: {e}"))
        }
    }
}
