//! Device/management plumbing that outlives any single firmware role: Home
//! Assistant MQTT discovery payloads, the build/identity object, OTA-URL
//! validation, and the WiFi-join retry backoff.

pub mod backoff;
pub mod discovery;
pub mod ota;
pub mod version;
