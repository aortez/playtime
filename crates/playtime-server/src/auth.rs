pub use playtime_local::{Credentials, Token};

use crate::application::{ApiError, Result};
use axum::http::{HeaderMap, header};

pub(crate) fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(ApiError::new("malformed_data"));
    }
    value
        .map(|v| v.to_str().map_err(|_| ApiError::new("malformed_data")))
        .transpose()
}

pub(crate) fn authenticate(headers: &HeaderMap, token: &Token) -> Result<()> {
    let auth = single_header(headers, header::AUTHORIZATION.as_str())
        .map_err(|_| ApiError::new("unauthenticated"))?;
    let candidate = auth
        .and_then(|a| a.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new("unauthenticated"))?;
    if !token.matches(candidate) {
        return Err(ApiError::new("unauthenticated"));
    }
    Ok(())
}

pub(crate) fn check_native_headers(headers: &HeaderMap) -> Result<()> {
    if headers.contains_key(header::ORIGIN) {
        return Err(ApiError::new("forbidden_origin"));
    }
    let host = single_header(headers, "host")
        .map_err(|_| ApiError::new("forbidden_host"))?
        .ok_or_else(|| ApiError::new("forbidden_host"))?;
    let port = ["localhost:", "127.0.0.1:", "[::1]:"]
        .iter()
        .find_map(|prefix| host.strip_prefix(prefix));
    let valid = port.is_some_and(|p| {
        !p.is_empty()
            && p.bytes().all(|c| c.is_ascii_digit())
            && p.parse::<u16>().is_ok_and(|p| p != 0)
    });
    if !valid {
        return Err(ApiError::new("forbidden_host"));
    }
    Ok(())
}
