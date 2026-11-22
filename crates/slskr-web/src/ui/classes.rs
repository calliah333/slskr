//! Class-name composition mirroring semantic-ui-react's `classNameBuilders`
//! (`useKeyOnly`, `useValueAndKey`, `useKeyOrValueAndKey`, `useWidthProp`, ...).

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};

/// A prop that is either a boolean flag or a string value
/// (semantic-ui-react `useKeyOrValueAndKey`, e.g. `compact` vs `compact="very"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyOrValue {
    #[default]
    Off,
    On,
    Value(&'static str),
}

impl KeyOrValue {
    /// True unless `Off`.
    pub fn is_set(self) -> bool {
        !matches!(self, KeyOrValue::Off)
    }

    /// The string value, if any.
    pub fn value(self) -> Option<&'static str> {
        match self {
            KeyOrValue::Value(v) => Some(v),
            _ => None,
        }
    }
}

impl From<bool> for KeyOrValue {
    fn from(value: bool) -> Self {
        if value {
            KeyOrValue::On
        } else {
            KeyOrValue::Off
        }
    }
}

impl From<&'static str> for KeyOrValue {
    fn from(value: &'static str) -> Self {
        KeyOrValue::Value(value)
    }
}

impl From<Option<&'static str>> for KeyOrValue {
    fn from(value: Option<&'static str>) -> Self {
        value.map_or(KeyOrValue::Off, KeyOrValue::Value)
    }
}

/// A width prop: a number (rendered as a word, e.g. `4` -> `four`) or a literal word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    Number(u32),
    Named(&'static str),
}

impl From<u32> for Width {
    fn from(value: u32) -> Self {
        Width::Number(value)
    }
}

impl From<&'static str> for Width {
    fn from(value: &'static str) -> Self {
        Width::Named(value)
    }
}

const NUMBER_WORDS: [&str; 16] = [
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
    "twelve", "thirteen", "fourteen", "fifteen", "sixteen",
];

impl Width {
    /// The class word for this width (`numberToWord`).
    pub fn word(self) -> String {
        match self {
            Width::Number(n) if (1..=16).contains(&n) => NUMBER_WORDS[n as usize - 1].to_string(),
            Width::Number(n) => n.to_string(),
            Width::Named(s) => s
                .parse::<u32>()
                .map_or_else(|_| s.to_string(), |n| Width::Number(n).word()),
        }
    }
}

/// Builder equivalent to the `classnames` `cx(...)` call semantic-ui-react uses.
#[derive(Clone, Debug, Default)]
pub struct Classes(String);

impl Classes {
    /// Starts with a first fragment (usually `"ui"`, may be empty).
    pub fn new(first: &str) -> Self {
        Classes::default().add(first)
    }

    /// Appends a fragment unless it is empty.
    pub fn add(mut self, part: &str) -> Self {
        if !part.is_empty() {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(part);
        }
        self
    }

    /// Appends an optional fragment.
    pub fn opt(self, part: Option<&str>) -> Self {
        match part {
            Some(p) => self.add(p),
            None => self,
        }
    }

    /// `useKeyOnly(on, key)`.
    pub fn key(self, on: bool, key: &str) -> Self {
        if on { self.add(key) } else { self }
    }

    /// `useValueAndKey(value, key)` -> `"{value} {key}"`.
    pub fn value_key(self, value: Option<&str>, key: &str) -> Self {
        match value {
            Some(v) if !v.is_empty() => self.add(v).add(key),
            _ => self,
        }
    }

    /// `useKeyOrValueAndKey(value, key)`.
    pub fn key_or_value(self, value: KeyOrValue, key: &str) -> Self {
        match value {
            KeyOrValue::Off => self,
            KeyOrValue::On => self.add(key),
            KeyOrValue::Value(v) => self.value_key(Some(v), key),
        }
    }

    /// `useTextAlignProp(value)`.
    pub fn text_align(self, value: Option<&str>) -> Self {
        match value {
            Some("justified") => self.add("justified"),
            other => self.value_key(other, "aligned"),
        }
    }

    /// `useVerticalAlignProp(value)`.
    pub fn vertical_align(self, value: Option<&str>) -> Self {
        self.value_key(value, "aligned")
    }

    /// `useWidthProp(value, widthClass, canEqual)`.
    pub fn width(self, value: Option<Width>, width_class: &str, can_equal: bool) -> Self {
        match value {
            None => self,
            Some(Width::Named("equal")) if can_equal => self.add("equal width"),
            Some(w) => {
                let word = w.word();
                if width_class.is_empty() {
                    self.add(&word)
                } else {
                    self.add(&word).add(width_class)
                }
            }
        }
    }

    /// `useMultipleProp(value, key)`.
    pub fn multiple(mut self, value: Option<&str>, key: &str) -> Self {
        if let Some(v) = value {
            let normalized = v
                .replace("large screen", "large-screen")
                .replace(" vertically", "-vertically");
            for prop in normalized.split(' ').filter(|p| !p.is_empty()) {
                self = self.add(&prop.replacen('-', " ", 1)).add(key);
            }
        }
        self
    }

    /// The finished class string.
    pub fn build(self) -> String {
        self.0
    }
}

impl From<Classes> for String {
    fn from(value: Classes) -> Self {
        value.0
    }
}

/// Viewport-relative bounding box of an element (`getBoundingClientRect`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// Reads `element.getBoundingClientRect()` without requiring the `DomRect` web-sys feature.
pub fn client_rect(element: &web_sys::Element) -> Rect {
    let read = |obj: &JsValue, key: &str| {
        js_sys::Reflect::get(obj, &JsValue::from_str(key))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    };
    let rect = js_sys::Reflect::get(element, &JsValue::from_str("getBoundingClientRect"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .and_then(|f| f.call0(element).ok());
    match rect {
        Some(r) => Rect {
            left: read(&r, "left"),
            top: read(&r, "top"),
            width: read(&r, "width"),
            height: read(&r, "height"),
        },
        None => Rect::default(),
    }
}

/// Runs an optional callback.
pub(crate) fn emit<T: 'static>(callback: Option<Callback<T>>, value: T) {
    if let Some(cb) = callback {
        cb.run(value);
    }
}

/// Adds (`on == true`) or removes the given classes on `document.body`.
pub(crate) fn toggle_body_classes(classes: &[&str], on: bool) {
    if let Some(body) = document().body() {
        let list = body.class_list();
        for class in classes {
            let _ = if on {
                list.add_1(class)
            } else {
                list.remove_1(class)
            };
        }
    }
}
