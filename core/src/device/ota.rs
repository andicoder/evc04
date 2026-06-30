//! Validate the `evc04/cn28/ota` payload (a firmware URL) at the MQTT boundary.
//!
//! Transport is **plain HTTP on a trusted LAN** (#76): the `.bin` is pulled from
//! a short-lived local server, so only `http://` is accepted — there is no TLS
//! stack baked into the firmware to honour `https://`. A malformed payload is
//! rejected here so the device never drives the OTA path with garbage.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtaUrlError {
    /// Scheme is not `http://` (e.g. empty, `https://`, or no scheme at all).
    NotHttp,
    /// `http://` with no host before the path (`http://` or `http:///x`).
    MissingHost,
}

/// Validate a firmware URL payload, returning the trimmed URL on success.
pub fn validate_ota_url(input: &str) -> Result<&str, OtaUrlError> {
    let url = input.trim();
    let rest = url.strip_prefix("http://").ok_or(OtaUrlError::NotHttp)?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err(OtaUrlError::MissingHost);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_plain_http_url() {
        assert_eq!(
            validate_ota_url("http://192.168.1.10:8000/fw.bin"),
            Ok("http://192.168.1.10:8000/fw.bin")
        );
    }

    #[test]
    fn accepts_host_with_no_path() {
        assert_eq!(validate_ota_url("http://host"), Ok("http://host"));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(
            validate_ota_url("  http://h/fw.bin\n"),
            Ok("http://h/fw.bin")
        );
    }

    #[test]
    fn rejects_https() {
        assert_eq!(
            validate_ota_url("https://h/fw.bin"),
            Err(OtaUrlError::NotHttp)
        );
    }

    #[test]
    fn rejects_missing_scheme() {
        assert_eq!(
            validate_ota_url("192.168.1.10/fw.bin"),
            Err(OtaUrlError::NotHttp)
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate_ota_url(""), Err(OtaUrlError::NotHttp));
    }

    #[test]
    fn rejects_scheme_only() {
        assert_eq!(validate_ota_url("http://"), Err(OtaUrlError::MissingHost));
    }

    #[test]
    fn rejects_empty_host() {
        assert_eq!(
            validate_ota_url("http:///fw.bin"),
            Err(OtaUrlError::MissingHost)
        );
    }
}
