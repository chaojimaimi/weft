// font-kit/src/sources/core_text.rs
//
// Copyright © 2018 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A source that contains the installed fonts on macOS.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_text::font_collection::{self, CTFontCollection};
use core_text::font_descriptor::{self, CTFontDescriptor};
use core_text::font_manager;
use std::any::Any;
use std::f32;
use std::path::Path;

use crate::error::SelectionError;
use crate::family_handle::FamilyHandle;
use crate::family_name::FamilyName;
use crate::handle::Handle;
use crate::loaders::core_text::{self as core_text_loader, FONT_WEIGHT_MAPPING};
use crate::properties::{Properties, Stretch, Weight};
use crate::source::Source;
use crate::utils;

/// A source that contains the installed fonts on macOS.
#[allow(missing_debug_implementations)]
#[allow(missing_copy_implementations)]
pub struct CoreTextSource;

impl CoreTextSource {
    /// Opens a new connection to the system font source.
    ///
    /// (Note that this doesn't actually do any Mach communication to the font server; that is done
    /// lazily on demand by the Core Text/Core Graphics API.)
    #[inline]
    pub fn new() -> CoreTextSource {
        CoreTextSource
    }

    /// Returns paths of all fonts installed on the system.
    pub fn all_fonts(&self) -> Result<Vec<Handle>, SelectionError> {
        let collection = font_collection::create_for_all_families();
        create_handles_from_core_text_collection(collection)
    }

    /// Returns the names of all families installed on the system.
    pub fn all_families(&self) -> Result<Vec<String>, SelectionError> {
        let core_text_family_names = font_manager::copy_available_font_family_names();
        let mut families = Vec::with_capacity(core_text_family_names.len() as usize);
        for core_text_family_name in core_text_family_names.iter() {
            families.push(core_text_family_name.to_string())
        }
        Ok(families)
    }

    /// Looks up a font family by name and returns the handles of all the fonts in that family.
    pub fn select_family_by_name(&self, family_name: &str) -> Result<FamilyHandle, SelectionError> {
        let attributes: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[(
            CFString::new("NSFontFamilyAttribute"),
            CFString::new(family_name).as_CFType(),
        )]);

        let descriptor = font_descriptor::new_from_attributes(&attributes);
        let descriptors = CFArray::from_CFTypes(&[descriptor]);
        let collection = font_collection::new_from_descriptors(&descriptors);
        let handles = create_handles_from_core_text_collection(collection)?;
        Ok(FamilyHandle::from_font_handles(handles.into_iter()))
    }

    /// Selects a font by PostScript name, which should be a unique identifier.
    pub fn select_by_postscript_name(
        &self,
        postscript_name: &str,
    ) -> Result<Handle, SelectionError> {
        let attributes: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[(
            CFString::new("NSFontNameAttribute"),
            CFString::new(postscript_name).as_CFType(),
        )]);

        let descriptor = font_descriptor::new_from_attributes(&attributes);
        let descriptors = CFArray::from_CFTypes(&[descriptor]);
        let collection = font_collection::new_from_descriptors(&descriptors);
        match collection.get_descriptors() {
            None => Err(SelectionError::NotFound),
            Some(descriptors) => create_handle_from_descriptor(&*descriptors.get(0).unwrap()),
        }
    }

    /// Performs font matching according to the CSS Fonts Level 3 specification and returns the
    /// handle.
    #[inline]
    pub fn select_best_match(
        &self,
        family_names: &[FamilyName],
        properties: &Properties,
    ) -> Result<Handle, SelectionError> {
        <Self as Source>::select_best_match(self, family_names, properties)
    }
}

impl Source for CoreTextSource {
    fn all_fonts(&self) -> Result<Vec<Handle>, SelectionError> {
        self.all_fonts()
    }

    fn all_families(&self) -> Result<Vec<String>, SelectionError> {
        self.all_families()
    }

    fn select_family_by_name(&self, family_name: &str) -> Result<FamilyHandle, SelectionError> {
        self.select_family_by_name(family_name)
    }

    fn select_by_postscript_name(&self, postscript_name: &str) -> Result<Handle, SelectionError> {
        self.select_by_postscript_name(postscript_name)
    }

    #[inline]
    fn as_any(&self) -> &dyn Any {
        self
    }

    #[inline]
    fn as_mut_any(&mut self) -> &mut dyn Any {
        self
    }
}

#[allow(dead_code)]
fn css_to_core_text_font_weight(css_weight: Weight) -> f32 {
    core_text_loader::piecewise_linear_lookup(
        f32::max(100.0, css_weight.0) / 100.0 - 1.0,
        &FONT_WEIGHT_MAPPING,
    )
}

#[allow(dead_code)]
fn css_stretchiness_to_core_text_width(css_stretchiness: Stretch) -> f32 {
    let css_stretchiness = utils::clamp(css_stretchiness.0, 0.5, 2.0);
    0.1 * core_text_loader::piecewise_linear_find_index(css_stretchiness, &Stretch::MAPPING) - 0.4
}

/// None-tolerant PostScript-name read (Weft vendored patch, review should-fix 1):
/// `CTFontDescriptor::font_name()` expects the attribute to exist and panics
/// otherwise; a malformed collection member must not abort the startup font
/// search. Returns `None` when the attribute is absent or not a string.
fn descriptor_postscript_name(descriptor: &CTFontDescriptor) -> Option<String> {
    unsafe {
        let value = font_descriptor::CTFontDescriptorCopyAttribute(
            descriptor.as_concrete_TypeRef(),
            font_descriptor::kCTFontNameAttribute,
        );
        if value.is_null() {
            return None;
        }
        let value = CFType::wrap_under_create_rule(value);
        value.downcast_into::<CFString>().map(|s| s.to_string())
    }
}

/// Returns the index of `descriptor` within the font file at `path`.
///
/// Weft vendored patch (P2): `font_index` matches the on-disk member order. For a single-font
/// file `CTFontManagerCreateFontDescriptorsFromURL` returns a one-element array, so the index is
/// 0; for a `.ttc`/`.otc` collection we compare PostScript names against the target descriptor.
/// A member without a readable PostScript name (or a name match failure) falls back to index 0
/// rather than skipping the font — documented divergence from upstream's skip-on-mismatch, see
/// PATCH.md.
fn font_index_for_descriptor(descriptor: &CTFontDescriptor, path: &Path) -> u32 {
    let Some(members) = core_text_loader::core_text_descriptors_from_url(path) else {
        return 0;
    };
    let Some(postscript_name) = descriptor_postscript_name(descriptor) else {
        return 0;
    };
    members
        .iter()
        .position(|member| {
            descriptor_postscript_name(&member).as_deref() == Some(postscript_name.as_str())
        })
        .map(|index| index as u32)
        .unwrap_or(0)
}

fn create_handles_from_core_text_collection(
    collection: CTFontCollection,
) -> Result<Vec<Handle>, SelectionError> {
    let mut fonts = vec![];
    if let Some(descriptors) = collection.get_descriptors() {
        for index in 0..descriptors.len() {
            let descriptor = descriptors.get(index).unwrap();
            // A descriptor whose backing file cannot be located cannot become a `Path` handle;
            // upstream skipped such fonts (open/slurp failure) rather than failing the search.
            let Some(font_path) = descriptor.font_path() else {
                continue;
            };
            let font_index = font_index_for_descriptor(&descriptor, &font_path);
            fonts.push(Handle::from_path(font_path, font_index));
        }
    }
    if fonts.is_empty() {
        Err(SelectionError::NotFound)
    } else {
        Ok(fonts)
    }
}

fn create_handle_from_descriptor(descriptor: &CTFontDescriptor) -> Result<Handle, SelectionError> {
    let font_path = descriptor.font_path().unwrap();
    let font_index = font_index_for_descriptor(&descriptor, &font_path);
    Ok(Handle::from_path(font_path, font_index))
}

#[cfg(test)]
mod test {
    use crate::properties::{Stretch, Weight};

    #[test]
    fn test_css_to_core_text_font_weight() {
        // Exact matches
        assert_eq!(super::css_to_core_text_font_weight(Weight(100.0)), -0.7);
        assert_eq!(super::css_to_core_text_font_weight(Weight(400.0)), 0.0);
        assert_eq!(super::css_to_core_text_font_weight(Weight(700.0)), 0.4);
        assert_eq!(super::css_to_core_text_font_weight(Weight(900.0)), 0.8);

        // Linear interpolation
        assert_eq!(super::css_to_core_text_font_weight(Weight(450.0)), 0.1);
    }

    #[test]
    fn test_css_to_core_text_font_stretch() {
        // Exact matches
        assert_eq!(
            super::css_stretchiness_to_core_text_width(Stretch(1.0)),
            0.0
        );
        assert_eq!(
            super::css_stretchiness_to_core_text_width(Stretch(0.5)),
            -0.4
        );
        assert_eq!(
            super::css_stretchiness_to_core_text_width(Stretch(2.0)),
            0.4
        );

        // Linear interpolation
        assert_eq!(
            super::css_stretchiness_to_core_text_width(Stretch(1.7)),
            0.34
        );
    }
}
