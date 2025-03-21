use actix_web::{http::header::Header, HttpRequest};
use actix_web_httpauth::headers::authorization::{Authorization, Bearer};
use base64::{engine::general_purpose::STANDARD_NO_PAD as base64, Engine};
use captcha::Captcha;
use lemmy_api_common::{
  claims::Claims,
  context::LemmyContext,
  utils::{check_user_valid, local_site_to_slur_regex, AUTH_COOKIE_NAME},
};
use lemmy_db_schema::source::local_site::LocalSite;
use lemmy_db_views::structs::LocalUserView;
use lemmy_utils::{
  error::{LemmyError, LemmyErrorExt, LemmyErrorExt2, LemmyErrorType, LemmyResult},
  utils::slurs::check_slurs,
};
use std::io::Cursor;
use totp_rs::{Secret, TOTP};

pub mod comment;
pub mod comment_report;
pub mod community;
pub mod local_user;
pub mod post;
pub mod post_report;
pub mod private_message;
pub mod private_message_report;
pub mod site;
pub mod sitemap;

/// Converts the captcha to a base64 encoded wav audio file
pub(crate) fn captcha_as_wav_base64(captcha: &Captcha) -> Result<String, LemmyError> {
  let letters = captcha.as_wav();

  // Decode each wav file, concatenate the samples
  let mut concat_samples: Vec<i16> = Vec::new();
  let mut any_header: Option<wav::Header> = None;
  for letter in letters {
    let mut cursor = Cursor::new(letter.unwrap_or_default());
    let (header, samples) = wav::read(&mut cursor)?;
    any_header = Some(header);
    if let Some(samples16) = samples.as_sixteen() {
      concat_samples.extend(samples16);
    } else {
      Err(LemmyErrorType::CouldntCreateAudioCaptcha)?
    }
  }

  // Encode the concatenated result as a wav file
  let mut output_buffer = Cursor::new(vec![]);
  if let Some(header) = any_header {
    wav::write(
      header,
      &wav::BitDepth::Sixteen(concat_samples),
      &mut output_buffer,
    )
    .with_lemmy_type(LemmyErrorType::CouldntCreateAudioCaptcha)?;

    Ok(base64.encode(output_buffer.into_inner()))
  } else {
    Err(LemmyErrorType::CouldntCreateAudioCaptcha)?
  }
}

/// Check size of report
pub(crate) fn check_report_reason(reason: &str, local_site: &LocalSite) -> Result<(), LemmyError> {
  let slur_regex = &local_site_to_slur_regex(local_site);

  check_slurs(reason, slur_regex)?;
  if reason.is_empty() {
    Err(LemmyErrorType::ReportReasonRequired)?
  } else if reason.chars().count() > 1000 {
    Err(LemmyErrorType::ReportTooLong)?
  } else {
    Ok(())
  }
}

pub fn read_auth_token(req: &HttpRequest) -> Result<Option<String>, LemmyError> {
  // Try reading jwt from auth header
  if let Ok(header) = Authorization::<Bearer>::parse(req) {
    Ok(Some(header.as_ref().token().to_string()))
  }
  // If that fails, try to read from cookie
  else if let Some(cookie) = &req.cookie(AUTH_COOKIE_NAME) {
    // ensure that its marked as httponly and secure
    let secure = cookie.secure().unwrap_or_default();
    let http_only = cookie.http_only().unwrap_or_default();
    let is_debug_mode = cfg!(debug_assertions);

    if !is_debug_mode && (!secure || !http_only) {
      Err(LemmyError::from(LemmyErrorType::AuthCookieInsecure))
    } else {
      Ok(Some(cookie.value().to_string()))
    }
  }
  // Otherwise, there's no auth
  else {
    Ok(None)
  }
}

pub(crate) fn check_totp_2fa_valid(
  local_user_view: &LocalUserView,
  totp_token: &Option<String>,
  site_name: &str,
) -> LemmyResult<()> {
  // Throw an error if their token is missing
  let token = totp_token
    .as_deref()
    .ok_or(LemmyErrorType::MissingTotpToken)?;
  let secret = local_user_view
    .local_user
    .totp_2fa_secret
    .as_deref()
    .ok_or(LemmyErrorType::MissingTotpSecret)?;

  let totp = build_totp_2fa(site_name, &local_user_view.person.name, secret)?;

  let check_passed = totp.check_current(token)?;
  if !check_passed {
    return Err(LemmyErrorType::IncorrectTotpToken.into());
  }

  Ok(())
}

pub(crate) fn generate_totp_2fa_secret() -> String {
  Secret::generate_secret().to_string()
}

pub(crate) fn build_totp_2fa(
  site_name: &str,
  username: &str,
  secret: &str,
) -> Result<TOTP, LemmyError> {
  let sec = Secret::Raw(secret.as_bytes().to_vec());
  let sec_bytes = sec
    .to_bytes()
    .map_err(|_| LemmyErrorType::CouldntParseTotpSecret)?;

  TOTP::new(
    totp_rs::Algorithm::SHA1,
    6,
    1,
    30,
    sec_bytes,
    Some(site_name.to_string()),
    username.to_string(),
  )
  .with_lemmy_type(LemmyErrorType::CouldntGenerateTotp)
}

#[tracing::instrument(skip_all)]
pub async fn local_user_view_from_jwt(
  jwt: &str,
  context: &LemmyContext,
) -> Result<LocalUserView, LemmyError> {
  let local_user_id = Claims::validate(jwt, context)
    .await
    .with_lemmy_type(LemmyErrorType::NotLoggedIn)?;
  let local_user_view = LocalUserView::read(&mut context.pool(), local_user_id).await?;
  check_user_valid(&local_user_view.person)?;

  Ok(local_user_view)
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]
  #![allow(clippy::indexing_slicing)]

  use super::*;

  #[test]
  fn test_build_totp() {
    let generated_secret = generate_totp_2fa_secret();
    let totp = build_totp_2fa("lemmy", "my_name", &generated_secret);
    assert!(totp.is_ok());
  }
}
