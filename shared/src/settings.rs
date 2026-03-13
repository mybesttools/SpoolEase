pub const SCALE_STABLE_OTA_PATH: &str = "/yanshay/SpoolEase/main/build/bins/0.6/scale/ota/";
pub const SCALE_UNSTABLE_OTA_PATH: &str = "/yanshay/SpoolEase/main/build/bins/0.6/scale/ota-unstable/";
pub const SCALE_DEBUG_OTA_PATH: &str = "/yanshay/SpoolEase/main/build/bins/0.6/scale/debug/";
pub const OTA_DOMAIN_STABLE: &str = "raw.githubusercontent.com";
pub const OTA_DOMAIN_UNSTABLE: &str = "raw.githubusercontent.com";
pub const OTA_DOMAIN_DEBUG: &str = "raw.githubusercontent.com";
pub const OTA_TLS_CERTIFICATE: &str = concat!(include_str!("./certs/raw.githubusercontent.com.pem.full"), "\0");
