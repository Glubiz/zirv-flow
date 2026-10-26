//! The look of a block email (architecture 11.3, 11.5): the colours, width, font and button and
//! link styles, from the document's `settings` or from the theme in effect.
//!
//! The crate resolves no theme itself. A theme is in effect only when the document's `theme` is
//! not `none` and the caller passed [`RenderExtras::theme`]; the footer closes the email only
//! then, too.

use serde_json::{Value, json};

use super::model::{BlocksContent, FooterRef, Settings, ThemeRef, font_css};
use super::snippet::SnippetContent;
use crate::context::{RenderContext, RenderExtras};
use crate::issue::Issue;
use crate::theme::{ThemeTokens, validate_theme};

/// The id of the section the theme footer becomes. No document may use it.
pub(crate) const THEME_FOOTER_ID: &str = "theme-footer";

const FALLBACK_STACK: &str = "Arial, Helvetica, sans-serif";

/// What a document refers to that the caller did not pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Broken {
    /// The document names a theme and none was passed.
    Theme,
    /// The theme's footer snippet has no content.
    Footer,
    /// The theme tokens fail `validate_theme`: a value that is not safe to put in MJML.
    Tokens,
}

/// The values the emitter applies. The extras that matter are borrowed.
pub(crate) struct Look<'a> {
    pub(crate) background: &'a str,
    pub(crate) text: &'a str,
    pub(crate) link: &'a str,
    pub(crate) button: &'a str,
    pub(crate) button_text: &'a str,
    pub(crate) content_width: u32,
    pub(crate) font_stack: &'static str,
    /// The heading colour; only a theme sets it.
    pub(crate) heading: Option<&'a str>,
    /// Whether links are underlined; only a theme sets it.
    pub(crate) link_underline: Option<bool>,
    /// The button corner radius in pixels; only a theme sets it.
    pub(crate) button_radius: Option<u32>,
    pub(crate) web_font_url: Option<&'a str>,
    /// The theme in effect.
    pub(crate) theme: Option<&'a ThemeTokens>,
    /// The blocks of the theme footer, appended as the last section.
    pub(crate) footer: Option<&'a SnippetContent>,
}

impl<'a> Look<'a> {
    /// The look of the document's own settings: what schema 1 rendered.
    pub(crate) fn of_settings(settings: &'a Settings) -> Self {
        Self {
            background: &settings.background_color,
            text: &settings.text_color,
            link: &settings.link_color,
            button: &settings.button_color,
            button_text: &settings.button_text_color,
            content_width: settings.content_width,
            font_stack: font_css(&settings.font_family).unwrap_or(FALLBACK_STACK),
            heading: None,
            link_underline: None,
            button_radius: None,
            web_font_url: None,
            theme: None,
            footer: None,
        }
    }

    /// The look of `content` under `extras`, and what it names that is missing. A missing theme
    /// leaves the settings in force; a missing footer leaves the theme without one.
    pub(crate) fn resolve(
        content: &'a BlocksContent,
        extras: &'a RenderExtras,
    ) -> (Self, Option<Broken>) {
        let mut look = Self::of_settings(&content.settings);
        let theme = match (&content.theme, &extras.theme) {
            (ThemeRef::None, _) | (ThemeRef::Default, None) => return (look, None),
            (ThemeRef::Id(_), None) => return (look, Some(Broken::Theme)),
            (_, Some(theme)) => theme,
        };
        if validate_theme(theme).iter().any(Issue::is_blocking) {
            return (look, Some(Broken::Tokens));
        }
        look.background = &theme.background;
        look.text = &theme.text;
        look.link = &theme.link;
        look.button = &theme.button;
        look.button_text = &theme.button_text;
        look.content_width = theme.content_width;
        look.font_stack = font_css(&theme.font).unwrap_or(FALLBACK_STACK);
        look.heading = Some(&theme.primary);
        look.link_underline = Some(theme.link_underline);
        look.button_radius = Some(theme.button_radius);
        look.web_font_url = theme.web_font_url.as_deref();
        look.theme = Some(theme);
        let mut broken = None;
        if content.footer == FooterRef::Theme
            && let Some(id) = &theme.footer_snippet
        {
            match extras.snippets.get(id) {
                Some(snippet) => look.footer = Some(snippet),
                None => broken = Some(Broken::Footer),
            }
        }
        (look, broken)
    }

    /// The `theme` merge values, when a theme is in effect.
    pub(crate) fn theme_values(&self, extras: &RenderExtras) -> Option<Value> {
        let theme = self.theme?;
        let mut values = json!({
            "primary": theme.primary,
            "text": theme.text,
            "background": theme.background,
            "link": theme.link,
            "button": theme.button,
            "button_text": theme.button_text,
            "name": extras.theme_name.clone().unwrap_or_default(),
        });
        if let (Some(url), Some(map)) = (extras.theme_logo_url(), values.as_object_mut()) {
            map.insert("logo_url".to_owned(), Value::from(url));
        }
        Some(values)
    }

    /// The template values of `context`, with `theme` added where a theme is in effect.
    pub(crate) fn values(
        &self,
        context: &RenderContext,
        extras: &RenderExtras,
    ) -> minijinja::Value {
        if let Some(theme) = self.theme_values(extras)
            && let Ok(Value::Object(mut map)) = serde_json::to_value(context)
        {
            map.insert("theme".to_owned(), theme);
            return minijinja::Value::from_serialize(Value::Object(map));
        }
        minijinja::Value::from_serialize(context)
    }

    /// The first family of the font stack: the name an `mj-font` carries.
    pub(crate) fn font_name(&self) -> &'static str {
        self.font_stack
            .split(',')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('\'')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNIPPET: &str = "0192b1a4-7c3e-7000-8000-00000000c001";

    fn theme() -> ThemeTokens {
        ThemeTokens {
            primary: "#aa0000".into(),
            text: "#00aa00".into(),
            background: "#0000aa".into(),
            content_width: 700,
            font: "georgia".into(),
            button_radius: 8,
            link_underline: false,
            footer_snippet: Some(SNIPPET.into()),
            ..ThemeTokens::default()
        }
    }

    fn extras(theme: Option<ThemeTokens>) -> RenderExtras {
        let mut extras = RenderExtras {
            theme,
            theme_name: Some("Brand".into()),
            ..RenderExtras::default()
        };
        extras
            .snippets
            .insert(SNIPPET.into(), SnippetContent::default());
        extras
    }

    fn content(theme: ThemeRef) -> BlocksContent {
        let mut content = BlocksContent::new("Hi");
        content.theme = theme;
        content
    }

    #[test]
    fn a_document_with_no_theme_keeps_its_settings_whatever_the_extras_hold() {
        let content = content(ThemeRef::None);
        let extras = extras(Some(theme()));
        let (look, broken) = Look::resolve(&content, &extras);
        assert_eq!(broken, None);
        assert!(look.theme.is_none() && look.footer.is_none());
        assert_eq!(look.background, content.settings.background_color);
        assert_eq!(look.content_width, 600);
        assert!(look.heading.is_none() && look.button_radius.is_none());
        assert!(look.theme_values(&extras).is_none());
    }

    #[test]
    fn a_followed_default_with_no_theme_in_the_extras_keeps_the_settings() {
        let content = content(ThemeRef::Default);
        let extras = RenderExtras::default();
        let (look, broken) = Look::resolve(&content, &extras);
        assert_eq!(broken, None);
        assert!(look.theme.is_none());
    }

    #[test]
    fn a_named_theme_with_no_theme_in_the_extras_is_broken() {
        let content = content(ThemeRef::Id(SNIPPET.into()));
        let none = RenderExtras::default();
        let (look, broken) = Look::resolve(&content, &none);
        assert_eq!(broken, Some(Broken::Theme));
        assert!(look.theme.is_none());
    }

    #[test]
    fn a_theme_in_effect_replaces_the_settings_and_brings_its_footer() {
        let content = content(ThemeRef::Default);
        let extras = extras(Some(theme()));
        let (look, broken) = Look::resolve(&content, &extras);
        assert_eq!(broken, None);
        assert_eq!(look.background, "#0000aa");
        assert_eq!(look.text, "#00aa00");
        assert_eq!(look.heading, Some("#aa0000"));
        assert_eq!(look.content_width, 700);
        assert_eq!(look.font_name(), "Georgia");
        assert_eq!(look.link_underline, Some(false));
        assert_eq!(look.button_radius, Some(8));
        assert!(look.footer.is_some());
    }

    #[test]
    fn the_footer_needs_its_content_and_the_footer_setting() {
        let mut content = content(ThemeRef::Default);
        let mut without = extras(Some(theme()));
        without.snippets.clear();
        assert_eq!(Look::resolve(&content, &without).1, Some(Broken::Footer));
        content.footer = FooterRef::None;
        assert_eq!(Look::resolve(&content, &without).1, None);
        let mut bare = theme();
        bare.footer_snippet = None;
        content.footer = FooterRef::Theme;
        let bare = extras(Some(bare));
        let (look, broken) = Look::resolve(&content, &bare);
        assert_eq!((look.footer.is_none(), broken), (true, None));
    }

    #[test]
    fn the_theme_values_are_the_colours_the_name_and_a_logo_url_only_with_a_logo() {
        let content = content(ThemeRef::Default);
        let mut extras = extras(Some(theme()));
        let (look, _) = Look::resolve(&content, &extras);
        let values = look.theme_values(&extras).unwrap();
        assert_eq!(values["primary"], "#aa0000");
        assert_eq!(values["name"], "Brand");
        assert!(values.get("logo_url").is_none());

        let logo = "0192b1a4-7c3e-7000-8000-00000000b001";
        extras.theme.as_mut().unwrap().logo = Some(logo.into());
        extras
            .assets
            .insert(logo.into(), "https://cdn.example/l.png".into());
        let (look, _) = Look::resolve(&content, &extras);
        let values = look.theme_values(&extras).unwrap();
        assert_eq!(values["logo_url"], "https://cdn.example/l.png");
        let merged = look.values(&RenderContext::default(), &extras);
        assert_eq!(
            merged.get_attr("theme").unwrap().get_attr("name").unwrap(),
            minijinja::Value::from("Brand")
        );
    }
}
