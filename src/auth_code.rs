use crate::error::{Result, TgError};
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};
use tdlib_rs::enums::{AuthenticationCodeType, AuthorizationState, ResendCodeReason};
use tdlib_rs::types::AuthenticationCodeInfo;

fn delivery(kind: &AuthenticationCodeType) -> &'static str {
    match kind {
        AuthenticationCodeType::TelegramMessage(_) => {
            "Telegram service message on another logged-in device"
        }
        AuthenticationCodeType::Sms(_) => "SMS",
        AuthenticationCodeType::SmsWord(_) => "SMS containing a word",
        AuthenticationCodeType::SmsPhrase(_) => "SMS containing a phrase",
        AuthenticationCodeType::Call(_) => "phone call",
        AuthenticationCodeType::FlashCall(_) => "flash call",
        AuthenticationCodeType::MissedCall(_) => "missed call",
        AuthenticationCodeType::Fragment(_) => "Fragment website for your collectible phone number",
        AuthenticationCodeType::FirebaseAndroid(_) => {
            "Firebase in the official Telegram Android app"
        }
        AuthenticationCodeType::FirebaseIos(_) => "Firebase in the official Telegram iOS app",
    }
}

pub fn describe(info: &AuthenticationCodeInfo) -> String {
    let mut text = format!(
        "Telegram reports code delivery via: {}.\n",
        delivery(&info.r#type)
    );
    match &info.next_type {
        Some(next) => text.push_str(&format!(
            "Next delivery method: {}. Resend available after {} seconds.\n",
            delivery(next),
            info.timeout.max(0)
        )),
        None => text.push_str("Telegram offers no next delivery method. Resend is unavailable.\n"),
    }
    text.push_str("Enter the code, '/status' to refresh delivery details, '/resend' to request the next method, or '/quit'.");
    text
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Code(String),
    Status,
    Resend,
    Refused(String),
    Quit,
}

fn action(input: &str, info: &AuthenticationCodeInfo, elapsed: Duration) -> Action {
    match input.trim() {
        "/status" => Action::Status,
        "/quit" => Action::Quit,
        "/resend" if info.next_type.is_none() => {
            Action::Refused("Telegram offers no next delivery method; no resend requested.".into())
        }
        "/resend" => {
            let remaining = Duration::from_secs(info.timeout.max(0) as u64).saturating_sub(elapsed);
            if remaining.is_zero() {
                Action::Resend
            } else {
                Action::Refused(format!(
                    "Wait {} seconds before resend; no resend requested.",
                    remaining.as_secs().saturating_add(1)
                ))
            }
        }
        "" => Action::Refused("No code entered; nothing submitted.".into()),
        code => Action::Code(code.to_owned()),
    }
}

/// Inspect the current TDLib state without requesting another code on startup.
/// Neither code information nor entered secrets are written to logs.
pub(crate) async fn prompt(client_id: i32) -> Result<()> {
    loop {
        let state = tokio::time::timeout(
            Duration::from_secs(30),
            tdlib_rs::functions::get_authorization_state(client_id),
        )
        .await
        .map_err(|_| TgError::AuthFailed("Timed out reading authentication state".into()))?
        .map_err(|e| TgError::AuthFailed(e.message))?;
        let AuthorizationState::WaitCode(state) = state else {
            return Ok(());
        };
        let info = state.code_info;
        let started = Instant::now();
        println!("{}", describe(&info));
        print!("Code or command: ");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().lock().read_line(&mut input)? == 0 {
            return Err(TgError::AuthFailed(
                "Input closed; login remains pending".into(),
            ));
        }
        match action(&input, &info, started.elapsed()) {
            Action::Status => continue,
            Action::Quit => return Err(TgError::AuthFailed("Login remains pending".into())),
            Action::Refused(reason) => println!("{reason}"),
            Action::Resend => {
                let result = tokio::time::timeout(
                    Duration::from_secs(30),
                    tdlib_rs::functions::resend_authentication_code(
                        Some(ResendCodeReason::UserRequest),
                        client_id,
                    ),
                )
                .await;
                match result {
                    Ok(Ok(())) => println!("Telegram accepted the resend request. Refreshing delivery details."),
                    Ok(Err(e)) => return Err(TgError::AuthFailed(format!("Resend refused by Telegram: {}", e.message))),
                    Err(_) => return Err(TgError::AuthFailed("Resend outcome unknown. Restart tg auth to inspect state; do not resend blindly.".into())),
                }
            }
            Action::Code(code) => {
                return tokio::time::timeout(
                    Duration::from_secs(30),
                    tdlib_rs::functions::check_authentication_code(code, client_id),
                )
                .await
                .map_err(|_| {
                    TgError::AuthFailed(
                        "Code submission outcome unknown. Restart tg auth to inspect state.".into(),
                    )
                })?
                .map_err(|e| TgError::AuthFailed(e.message));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_code_reports_delivery_and_resend_without_private_details() {
        let info: AuthenticationCodeInfo = serde_json::from_value(json!({
            "phone_number": "+1234567890",
            "type": {"@type": "authenticationCodeTypeSms", "length": 5},
            "next_type": {"@type": "authenticationCodeTypeCall", "length": 5},
            "timeout": 42
        }))
        .unwrap();
        let text = describe(&info);
        assert!(text.contains("SMS"), "{text}");
        assert!(text.contains("phone call"), "{text}");
        assert!(text.contains("42"), "{text}");
        assert!(!text.contains("+1234567890"));
    }

    fn pending_code(timeout: i32, next: bool) -> AuthenticationCodeInfo {
        serde_json::from_value(json!({
            "phone_number": "+1234567890",
            "type": {"@type": "authenticationCodeTypeTelegramMessage", "length": 5},
            "next_type": if next { json!({"@type": "authenticationCodeTypeSms", "length": 5}) } else { json!(null) },
            "timeout": timeout
        })).unwrap()
    }

    #[test]
    fn auth_code_requires_next_delivery_method_even_after_timeout() {
        let info = pending_code(0, false);
        assert!(describe(&info).contains("Resend is unavailable"));
        assert!(matches!(
            action("/resend", &info, Duration::from_secs(60)),
            Action::Refused(_)
        ));
    }

    #[test]
    fn auth_code_waits_for_deadline_then_allows_explicit_resend() {
        let info = pending_code(42, true);
        assert!(matches!(
            action("/resend", &info, Duration::from_secs(41)),
            Action::Refused(_)
        ));
        assert_eq!(
            action("/resend", &info, Duration::from_secs(42)),
            Action::Resend
        );
        assert_eq!(
            action("/status", &info, Duration::from_secs(42)),
            Action::Status
        );
        assert_eq!(action("/quit", &info, Duration::ZERO), Action::Quit);
        assert!(matches!(
            action("", &info, Duration::ZERO),
            Action::Refused(_)
        ));
        assert_eq!(
            action(" 12345 \n", &info, Duration::ZERO),
            Action::Code("12345".into())
        );
        assert_eq!(
            action("status", &info, Duration::ZERO),
            Action::Code("status".into())
        );
    }

    #[test]
    fn auth_code_reports_telegram_and_handles_nonpositive_timeout() {
        let info = pending_code(-1, true);
        assert!(describe(&info).contains("another logged-in device"));
        assert!(describe(&info).contains("after 0 seconds"));
        assert_eq!(action("/resend", &info, Duration::ZERO), Action::Resend);
    }

    #[test]
    fn auth_code_does_not_print_fragment_token_or_url() {
        let mut info = pending_code(0, false);
        info.r#type = serde_json::from_value(json!({
            "@type": "authenticationCodeTypeFragment", "length": 5,
            "url": "https://fragment.com/login/PRIVATE_TOKEN"
        }))
        .unwrap();
        let text = describe(&info);
        assert!(text.contains("Fragment"));
        assert!(!text.contains("PRIVATE_TOKEN"));
        assert!(!text.contains("+1234567890"));
    }
}
