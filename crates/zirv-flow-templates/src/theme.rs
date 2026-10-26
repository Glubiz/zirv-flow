//! The theme tokens a block email can follow (architecture 11.3) and their pure validator.
//!
//! The crate stores nothing: a caller loads a theme and passes its tokens in
//! [`crate::RenderExtras`]. [`validate_theme`] checks the shapes and gives the contrast warnings
//! of OQ-22 (WCAG AA); it runs in the render pool like every other check.

use serde::{Deserialize, Serialize};

use crate::content::blocks::{
    FONT_STACKS, MAX_CONTENT_WIDTH, MAX_URL_BYTES, MIN_CONTENT_WIDTH, Settings, font_css, is_color,
};
use crate::issue::{Issue, code};
use crate::validate::is_uuid;

/// The widest button corner radius, in pixels.
pub const MAX_BUTTON_RADIUS: u32 = 40;

/// The contrast ratio WCAG AA asks of normal-size text.
const NORMAL_TEXT_RATIO: f64 = 4.5;
/// The contrast ratio WCAG AA asks of large text.
const LARGE_TEXT_RATIO: f64 = 3.0;

/// The design tokens of one theme.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ThemeTokens {
    /// The colour of headings and accents (large text on the background).
    pub primary: String,
    pub text: String,
    pub background: String,
    pub link: String,
    pub button: String,
    pub button_text: String,
    /// Pixels, 480 to 800.
    pub content_width: u32,
    /// A name from [`FONT_STACKS`].
    pub font: String,
    /// An optional `https` URL of a web font stylesheet; the stack above is the fallback.
    pub web_font_url: Option<String>,
    /// Pixels, 0 to 40.
    pub button_radius: u32,
    pub link_underline: bool,
    /// The id of the logo asset.
    pub logo: Option<String>,
    /// The id of the footer snippet.
    pub footer_snippet: Option<String>,
}

impl Default for ThemeTokens {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            primary: settings.link_color.clone(),
            text: settings.text_color,
            background: settings.background_color,
            link: settings.link_color,
            button: settings.button_color,
            button_text: settings.button_text_color,
            content_width: settings.content_width,
            font: settings.font_family,
            web_font_url: None,
            button_radius: 0,
            link_underline: true,
            logo: None,
            footer_snippet: None,
        }
    }
}

/// The relative luminance of `#rgb` or `#rrggbb` (WCAG 2): `None` for any other text.
fn luminance(color: &str) -> Option<f64> {
    if !is_color(color) {
        return None;
    }
    let digits = color.strip_prefix('#')?;
    let channel = |index: usize| -> Option<f64> {
        let value = if digits.len() == 3 {
            let nibble = u8::from_str_radix(digits.get(index..=index)?, 16).ok()?;
            nibble * 17
        } else {
            u8::from_str_radix(digits.get(index * 2..index * 2 + 2)?, 16).ok()?
        };
        let scaled = f64::from(value) / 255.0;
        Some(if scaled <= 0.039_28 {
            scaled / 12.92
        } else {
            ((scaled + 0.055) / 1.055).powf(2.4)
        })
    };
    Some(0.2126 * channel(0)? + 0.7152 * channel(1)? + 0.0722 * channel(2)?)
}

/// The WCAG contrast ratio of two colours, from 1 to 21; `None` when either is not a colour.
pub fn contrast_ratio(foreground: &str, background: &str) -> Option<f64> {
    let one = luminance(foreground)?;
    let two = luminance(background)?;
    let (light, dark) = if one >= two { (one, two) } else { (two, one) };
    Some((light + 0.05) / (dark + 0.05))
}

const fn asked_ratio(large_text: bool) -> f64 {
    if large_text {
        LARGE_TEXT_RATIO
    } else {
        NORMAL_TEXT_RATIO
    }
}

/// Whether `ratio` is below what AA asks of the text: 4.5 to 1, or 3 to 1 for large text.
fn below_aa(ratio: f64, large_text: bool) -> bool {
    ratio < asked_ratio(large_text)
}

fn color_issue(token: &str) -> Issue {
    Issue::new(code::INVALID_SETTING, format!("/theme/{token}"))
        .with_message("a colour as `#rgb` or `#rrggbb`")
}

fn contrast_issue(
    token: &str,
    foreground: &str,
    background: &str,
    large_text: bool,
    issues: &mut Vec<Issue>,
) {
    if let Some(ratio) = contrast_ratio(foreground, background)
        && below_aa(ratio, large_text)
    {
        issues.push(
            Issue::new(code::LOW_CONTRAST, format!("/theme/{token}")).with_message(format!(
                "contrast {ratio:.2}:1, WCAG AA asks {}:1",
                asked_ratio(large_text)
            )),
        );
    }
}

/// Check the shape of the tokens and warn where a colour pair falls below WCAG AA. Heading
/// colour (`primary`) is large text and needs 3:1; text, links and button text need 4.5:1. The
/// result is in a fixed order, shape issues first.
pub fn validate_theme(theme: &ThemeTokens) -> Vec<Issue> {
    let mut issues = Vec::new();
    for (token, value) in [
        ("primary", &theme.primary),
        ("text", &theme.text),
        ("background", &theme.background),
        ("link", &theme.link),
        ("button", &theme.button),
        ("button_text", &theme.button_text),
    ] {
        if !is_color(value) {
            issues.push(color_issue(token));
        }
    }
    if !(MIN_CONTENT_WIDTH..=MAX_CONTENT_WIDTH).contains(&theme.content_width) {
        issues.push(
            Issue::new(code::INVALID_SETTING, "/theme/content_width")
                .with_message(format!("{MIN_CONTENT_WIDTH} to {MAX_CONTENT_WIDTH} pixels")),
        );
    }
    if font_css(&theme.font).is_none() {
        let names: Vec<&str> = FONT_STACKS.iter().map(|(name, _)| *name).collect();
        issues.push(
            Issue::new(code::INVALID_SETTING, "/theme/font")
                .with_message(format!("one of {}", names.join(", "))),
        );
    }
    if let Some(url) = &theme.web_font_url
        && (!url.starts_with("https://")
            || url.len() > MAX_URL_BYTES
            || url.contains(char::is_whitespace))
    {
        issues.push(
            Issue::new(code::INVALID_URL, "/theme/web_font_url")
                .with_message("an https URL without spaces"),
        );
    }
    if theme.button_radius > MAX_BUTTON_RADIUS {
        issues.push(
            Issue::new(code::INVALID_SETTING, "/theme/button_radius")
                .with_message(format!("0 to {MAX_BUTTON_RADIUS} pixels")),
        );
    }
    for (token, id) in [
        ("logo", &theme.logo),
        ("footer_snippet", &theme.footer_snippet),
    ] {
        if id.as_deref().is_some_and(|id| !is_uuid(id)) {
            issues.push(
                Issue::new(code::INVALID_SETTING, format!("/theme/{token}")).with_message("an id"),
            );
        }
    }
    contrast_issue(
        "primary",
        &theme.primary,
        &theme.background,
        true,
        &mut issues,
    );
    contrast_issue("text", &theme.text, &theme.background, false, &mut issues);
    contrast_issue("link", &theme.link, &theme.background, false, &mut issues);
    contrast_issue(
        "button_text",
        &theme.button_text,
        &theme.button,
        false,
        &mut issues,
    );
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(issues: &[Issue]) -> Vec<(&'static str, &str)> {
        issues
            .iter()
            .map(|issue| (issue.code, issue.at.as_str()))
            .collect()
    }

    #[test]
    fn the_default_theme_is_valid_and_has_no_warning() {
        assert_eq!(validate_theme(&ThemeTokens::default()), Vec::new());
    }

    #[test]
    fn the_ratio_follows_the_wcag_formula() {
        let black_on_white = contrast_ratio("#000", "#ffffff").unwrap();
        assert!((black_on_white - 21.0).abs() < 1e-9);
        assert!((contrast_ratio("#fff", "#fff").unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(contrast_ratio("red", "#fff"), None);
        assert_eq!(
            contrast_ratio("#767676", "#fff"),
            contrast_ratio("#fff", "#767676")
        );
    }

    #[test]
    fn a_pair_at_4_4_warns_and_at_4_5_does_not_and_large_text_needs_only_3() {
        assert!(below_aa(4.4, false));
        assert!(!below_aa(4.5, false));
        assert!(!below_aa(3.0, true));
        assert!(below_aa(2.9, true));
        // Real colours either side of the line, on white.
        let mut theme = ThemeTokens {
            text: "#777777".into(),
            ..ThemeTokens::default()
        };
        assert_eq!(
            codes(&validate_theme(&theme)),
            [("low_contrast", "/theme/text")]
        );
        theme.text = "#767676".into();
        assert_eq!(validate_theme(&theme), Vec::new());
    }

    #[test]
    fn only_the_heading_colour_is_judged_as_large_text() {
        // About 3.5:1 on white: fine for headings, too low for links.
        let grey = "#8a8a8a";
        let ratio = contrast_ratio(grey, "#ffffff").unwrap();
        assert!((3.0..4.5).contains(&ratio), "{ratio}");
        let theme = ThemeTokens {
            primary: grey.into(),
            link: grey.into(),
            ..ThemeTokens::default()
        };
        assert_eq!(
            codes(&validate_theme(&theme)),
            [("low_contrast", "/theme/link")]
        );
        let button = ThemeTokens {
            button_text: grey.into(),
            ..ThemeTokens::default()
        };
        assert_eq!(
            codes(&validate_theme(&button)),
            [("low_contrast", "/theme/button_text")]
        );
        assert!(!validate_theme(&button)[0].is_blocking());
    }

    #[test]
    fn shapes_are_checked_with_their_paths() {
        let theme = ThemeTokens {
            primary: "blue".into(),
            content_width: 900,
            font: "comic".into(),
            web_font_url: Some("http://fonts.example/a.css".into()),
            button_radius: 99,
            logo: Some("not-an-id".into()),
            footer_snippet: Some("0192b1a4-7c3e-7000-8000-00000000a001".into()),
            ..ThemeTokens::default()
        };
        assert_eq!(
            codes(&validate_theme(&theme)),
            [
                ("invalid_setting", "/theme/primary"),
                ("invalid_setting", "/theme/content_width"),
                ("invalid_setting", "/theme/font"),
                ("invalid_url", "/theme/web_font_url"),
                ("invalid_setting", "/theme/button_radius"),
                ("invalid_setting", "/theme/logo"),
            ]
        );
    }

    #[test]
    fn tokens_read_with_defaults_and_refuse_an_unknown_one() {
        let theme: ThemeTokens = serde_json::from_str("{\"text\": \"#000000\"}").unwrap();
        assert_eq!(theme.text, "#000000");
        assert_eq!(theme.content_width, 600);
        assert!(serde_json::from_str::<ThemeTokens>("{\"shadow\": 1}").is_err());
    }
}
