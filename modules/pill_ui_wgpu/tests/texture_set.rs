//! The texture set's reload rule: a painter rebuilt mid-run re-uploads what
//! the carried set names, and a painter that already applied a revision does
//! not upload it again (W-0028 Stage 2, decision D3).
//!
//! # Responsibilities
//!
//! - Prove a second `UiRenderer` built from scratch uploads every texture the
//!   first one applied, from the same set.
//! - Prove the revision table is the upload decision: the same set twice is
//!   one upload, a newer revision is another, and a free forgets the entry.

// External crates
use pill_ui::{Region, TextureId, UiTexture, UiTextureFilter, UiTextureSet, UiTextureUpload};
use pill_ui_wgpu::offscreen;
use pill_ui_wgpu::renderer::UiRenderer;
use std::sync::Arc;

/// The target format the tests' renderers are built for.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[test]
fn a_rebuilt_painter_re_uploads_the_carried_set() {
    let Some((device, queue)) = block_on(offscreen::request_device(true))
        .or_else(|| block_on(offscreen::request_device(false)))
    else {
        eprintln!("no wgpu adapter is available; skipping the texture-set test");
        return;
    };
    let set = carried_set(3);

    // The first painter applies everything and records it.
    let mut first = UiRenderer::new(&device, &queue, FORMAT, (16, 16));
    first.apply_texture_set(&device, &queue, &set);
    assert_eq!(first.applied_revision(TextureId::Atlas), Some(3));
    assert_eq!(first.applied_revision(TextureId::External(7)), Some(3));
    drop(first);

    // A rebuilt painter remembers nothing, so the same set uploads again.
    let mut rebuilt = UiRenderer::new(&device, &queue, FORMAT, (16, 16));
    assert_eq!(
        rebuilt.applied_revision(TextureId::Atlas),
        None,
        "a fresh renderer must not claim a revision it never applied"
    );
    rebuilt.apply_texture_set(&device, &queue, &set);
    assert_eq!(rebuilt.applied_revision(TextureId::Atlas), Some(3));
    assert_eq!(rebuilt.applied_revision(TextureId::External(7)), Some(3));

    // The same revisions again are a no-op, by the table's own rule.
    rebuilt.apply_texture_set(&device, &queue, &set);
    assert_eq!(rebuilt.applied_revision(TextureId::Atlas), Some(3));

    // A newer revision is applied; a free forgets the texture.
    let newer = carried_set(4);
    rebuilt.apply_texture_set(&device, &queue, &newer);
    assert_eq!(rebuilt.applied_revision(TextureId::Atlas), Some(4));

    let mut freed = UiTextureSet::new();
    freed.push_free(TextureId::External(7));
    rebuilt.apply_texture_set(&device, &queue, &freed);
    assert_eq!(rebuilt.applied_revision(TextureId::External(7)), None);
    assert_eq!(
        rebuilt.applied_revision(TextureId::Atlas),
        Some(4),
        "freeing one texture must not forget the others"
    );
}

#[test]
fn a_patch_without_a_full_image_rebinds_nothing_on_a_fresh_painter() {
    let Some((device, queue)) = block_on(offscreen::request_device(true))
        .or_else(|| block_on(offscreen::request_device(false)))
    else {
        eprintln!("no wgpu adapter is available; skipping the texture-set test");
        return;
    };
    // A patch-only set (the producer's cheap path for a live painter) cannot
    // conjure a texture: the revision is recorded, and the draw is skipped.
    let mut set = UiTextureSet::new();
    set.push_patch(
        TextureId::External(9),
        8,
        8,
        1,
        UiTextureFilter::Linear,
        Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        },
        Arc::from(vec![200u8; 16]),
    );
    let mut painter = UiRenderer::new(&device, &queue, FORMAT, (16, 16));
    painter.apply_texture_set(&device, &queue, &set);
    assert_eq!(painter.applied_revision(TextureId::External(9)), Some(1));
}

// =============================================================================
// Fixtures
// =============================================================================

/// A set carrying the atlas and one external texture, both as whole images at
/// `revision`.
fn carried_set(revision: u64) -> UiTextureSet {
    let mut set = UiTextureSet::new();
    set.textures.push(UiTexture {
        id: TextureId::Atlas,
        width: 4,
        height: 4,
        revision,
        uploads: vec![UiTextureUpload {
            region: None,
            pixels: Arc::from(vec![180u8; 4 * 4 * 4]),
        }],
        filter: UiTextureFilter::Nearest,
    });
    set.textures.push(UiTexture {
        id: TextureId::External(7),
        width: 2,
        height: 2,
        revision,
        uploads: vec![UiTextureUpload {
            region: None,
            pixels: Arc::from(vec![90u8; 2 * 2 * 4]),
        }],
        filter: UiTextureFilter::Linear,
    });
    set
}

/// Block on `future` with the platform's executor.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    pill_core::platform::futures::block_on(future)
}
