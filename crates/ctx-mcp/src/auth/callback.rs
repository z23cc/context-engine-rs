use super::util::percent_decode;
use super::*;

pub(super) struct OAuthCallback {
    pub(super) code: Option<String>,
    pub(super) state: Option<String>,
    pub(super) error: Option<String>,
    pub(super) error_description: Option<String>,
    pub(super) manual_paste: bool,
}

#[derive(Debug)]
pub(super) struct LoopbackServer {
    listener: TcpListener,
    pub(super) redirect_uri: String,
}
pub(super) fn start_loopback_server() -> Result<LoopbackServer> {
    let listener = TcpListener::bind((REDIRECT_HOST, REDIRECT_PORT))
        .or_else(|_| TcpListener::bind((REDIRECT_HOST, 0)))
        .context("failed to bind xAI OAuth loopback listener")?;
    listener
        .set_nonblocking(true)
        .context("failed to configure xAI OAuth loopback listener")?;
    let port = listener.local_addr()?.port();
    Ok(LoopbackServer {
        listener,
        redirect_uri: format!("http://{REDIRECT_HOST}:{port}{REDIRECT_PATH}"),
    })
}

pub(super) fn wait_for_callback(
    server: LoopbackServer,
    timeout: Duration,
    expected_state: &str,
) -> Result<OAuthCallback> {
    let deadline = Instant::now() + timeout;
    loop {
        match server.listener.accept() {
            Ok((mut stream, _addr)) => {
                if let Some(callback) = handle_callback_stream(&mut stream, expected_state)? {
                    return Ok(callback);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for xAI OAuth callback");
                }
                sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("failed while waiting for xAI OAuth callback"),
        }
    }
}

pub(super) fn handle_callback_stream(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<Option<OAuthCallback>> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("failed to configure OAuth callback read timeout")?;
    let mut buffer = [0_u8; 8192];
    let read = match stream.read(&mut buffer) {
        Ok(read) => read,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err).context("failed to read OAuth callback"),
    };
    let request = String::from_utf8_lossy(&buffer[..read]);
    let Some((method, target)) = request_line_parts(&request) else {
        write_http_response(stream, 400, "invalid OAuth callback request")?;
        return Ok(None);
    };
    if method == "OPTIONS" {
        write_http_response(stream, 204, "")?;
        return Ok(None);
    }
    if method != "GET" {
        write_http_response(stream, 405, "method not allowed")?;
        return Ok(None);
    }
    if target.split('?').next() != Some(REDIRECT_PATH) {
        write_http_response(stream, 404, "not found")?;
        return Ok(None);
    }
    if !target.contains('?') {
        write_http_response(stream, 400, "OAuth callback missing query parameters")?;
        return Ok(None);
    }
    let callback = parse_callback_target(target, false)?;
    let terminal = callback.error.is_some() || callback.code.is_some();
    if !terminal {
        write_http_response(stream, 400, "OAuth callback missing code or error")?;
        return Ok(None);
    }
    if callback.state.as_deref() != Some(expected_state) {
        write_http_response(stream, 400, "OAuth callback state mismatch")?;
        return Ok(None);
    }
    write_http_response(
        stream,
        200,
        "ctx-mcp login complete; return to the terminal",
    )?;
    Ok(Some(callback))
}

pub(super) fn request_line_parts(request: &str) -> Option<(&str, &str)> {
    let mut parts = request.lines().next()?.split_whitespace();
    Some((parts.next()?, parts.next()?))
}

pub(super) fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    message: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let body = if status == 204 {
        String::new()
    } else {
        format!("<html><body><p>{message}</p></body></html>")
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Allow-Private-Network: true\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

pub(super) fn prompt_manual_callback() -> Result<OAuthCallback> {
    println!();
    println!("Paste the full callback URL or authorization code, then press Enter:");
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read callback URL")?;
    parse_pasted_callback(input.trim())
}

pub(super) fn parse_pasted_callback(input: &str) -> Result<OAuthCallback> {
    if input.contains("code=") || input.contains("error=") {
        let target = input
            .split_once(REDIRECT_HOST)
            .map_or(input, |(_, tail)| tail);
        let path = target.find('/').map_or(target, |idx| &target[idx..]);
        return parse_callback_target(path, true);
    }
    Ok(OAuthCallback {
        code: Some(input.trim().to_string()),
        state: None,
        error: None,
        error_description: None,
        manual_paste: true,
    })
}

pub(super) fn parse_callback_target(target: &str, manual_paste: bool) -> Result<OAuthCallback> {
    let (_, query) = target
        .split_once('?')
        .ok_or_else(|| anyhow!("OAuth callback did not include query parameters"))?;
    let params = parse_query(query)?;
    Ok(OAuthCallback {
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error").cloned(),
        error_description: params.get("error_description").cloned(),
        manual_paste,
    })
}

pub(super) fn validate_callback(callback: &OAuthCallback, expected_state: &str) -> Result<()> {
    if let Some(error) = &callback.error {
        let description = callback.error_description.as_deref().unwrap_or(error);
        bail!("xAI authorization failed: {description}");
    }
    if callback.state.as_deref() == Some(expected_state) {
        return Ok(());
    }
    if callback.manual_paste && callback.state.is_none() {
        return Ok(());
    }
    bail!("xAI authorization failed: state mismatch")
}

pub(super) fn parse_query(query: &str) -> Result<BTreeMap<String, String>> {
    let mut params = BTreeMap::new();
    for part in query.split('&').filter(|value| !value.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        params.insert(percent_decode(key)?, percent_decode(value)?);
    }
    Ok(params)
}

pub(super) fn try_open_browser(url: &str) {
    let result = browser_command(url)
        .and_then(|(program, args)| Command::new(program).args(args).status().ok());
    if !matches!(result, Some(status) if status.success()) {
        println!("Could not open the browser automatically; use the URL above.");
    }
}

pub(super) fn browser_command(url: &str) -> Option<(&'static str, Vec<&str>)> {
    #[cfg(target_os = "macos")]
    {
        Some(("open", vec![url]))
    }
    #[cfg(target_os = "windows")]
    {
        Some(("cmd", vec!["/C", "start", "", url]))
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        Some(("xdg-open", vec![url]))
    }
}
