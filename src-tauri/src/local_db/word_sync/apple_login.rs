//! The Apple Music sign-in window: gets the user's Media-User-Token without
//! a trip through the browser's developer tools.
//!
//! The settings page's "Sign in" button opens a second window of the app at
//! music.apple.com, in incognito mode: its own cookie store, kept in memory
//! only, shared with nothing (not the main window, not the user's browsers).
//! The user signs in there, with Apple's own pages in the system web engine;
//! the password and any second factor go from that page to Apple and never
//! pass through Lavender. Once the web player has a session, Apple sets the
//! `media-user-token` cookie on music.apple.com. This module polls the
//! window's cookies for it, hands the value to the settings page (which
//! stores it in `fork_config.apple_user_token`, as a pasted token is), then
//! wipes the window's browsing data and closes it. Nothing else of the
//! session survives: not Apple's other cookies, not the page cache.
//!
//! Cookies are read on a blocking thread, since WebView2 deadlocks when
//! they are read from a synchronous command. Closing the window before
//! signing in ends the attempt, and so does a ten-minute wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use super::apple::WEB_URL;

/// The label of the sign-in window; one at a time.
pub const LABEL: &str = "apple-sign-in";
/// The cookie Apple's web player carries its user token in.
pub const COOKIE: &str = "media-user-token";
/// How often the window's cookies are looked at.
pub const POLL: Duration = Duration::from_secs(1);
/// How long a sign-in may take before the window is closed.
pub const PATIENCE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignInError {
    #[error("the sign-in window is already open")]
    AlreadyOpen,
    #[error("the sign-in window was closed before Apple Music signed you in")]
    Closed,
    #[error("Apple Music did not sign you in within ten minutes; the window was closed")]
    TimedOut,
    #[error("the sign-in window could not be opened: {0}")]
    Window(String),
}

/// The user token among a site's cookies, if the web player has set one.
pub fn user_token<'a>(cookies: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<String> {
    cookies
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(COOKIE))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn site() -> Url {
    Url::parse(WEB_URL).expect("the Apple Music web address parses")
}

/// Opens the window and waits for the token, the window's closing or the
/// deadline, whichever comes first.
pub async fn run(app: &AppHandle) -> Result<String, SignInError> {
    if let Some(existing) = app.get_webview_window(LABEL) {
        let _ = existing.set_focus();
        return Err(SignInError::AlreadyOpen);
    }
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(site()))
        .title("Sign in to Apple Music")
        .inner_size(1040.0, 780.0)
        .incognito(true)
        .build()
        .map_err(|error| SignInError::Window(error.to_string()))?;

    let closed = Arc::new(AtomicBool::new(false));
    {
        let closed = closed.clone();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed) {
                closed.store(true, Ordering::SeqCst);
            }
        });
    }

    let started = Instant::now();
    loop {
        tokio::time::sleep(POLL).await;
        if closed.load(Ordering::SeqCst) {
            return Err(SignInError::Closed);
        }
        if let Some(token) = read_token(&window).await {
            finish(&window);
            return Ok(token);
        }
        if started.elapsed() >= PATIENCE {
            finish(&window);
            return Err(SignInError::TimedOut);
        }
    }
}

/// The token among the window's cookies for music.apple.com, read off the
/// async runtime. A cookie store that cannot be read yet counts as no token.
async fn read_token(window: &WebviewWindow) -> Option<String> {
    let window = window.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cookies = window.cookies_for_url(site()).ok()?;
        user_token(cookies.iter().map(|cookie| (cookie.name(), cookie.value())))
    })
    .await
    .ok()
    .flatten()
}

/// Wipes what the sign-in left in the window and closes it.
fn finish(window: &WebviewWindow) {
    let _ = window.clear_all_browsing_data();
    let _ = window.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_token_is_the_media_user_token_cookie() {
        let cookies = [
            ("geo", "US"),
            ("media-user-token", " AgAAAABzaWduZWQtaW4= "),
            ("itspod", "11"),
        ];
        assert_eq!(user_token(cookies), Some("AgAAAABzaWduZWQtaW4=".to_owned()));
    }

    #[test]
    fn the_cookie_name_is_matched_regardless_of_case() {
        assert_eq!(
            user_token([("Media-User-Token", "AgAAAAB0b2tlbg==")]),
            Some("AgAAAAB0b2tlbg==".to_owned())
        );
    }

    #[test]
    fn without_the_cookie_or_with_it_blank_there_is_no_token() {
        assert_eq!(user_token([("geo", "US"), ("itspod", "11")]), None);
        assert_eq!(user_token([("media-user-token", "  ")]), None);
        assert_eq!(user_token([]), None);
    }

    #[test]
    fn the_sign_in_window_opens_the_web_player() {
        assert_eq!(site().as_str(), "https://music.apple.com/");
        assert_eq!(site().host_str(), Some("music.apple.com"));
    }
}
