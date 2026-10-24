# Master Renderer Test

A PBR test project for `pill_master_renderer`. It draws the committed
DamagedHelmet through the renderer's own chain - a lit pass into a half-float
target, bloom, tonemap and a lens pass - and lets the player turn the helmet and
move the camera.

## Running

From `modules`:

```powershell
$env:PROJECT_PATH = "../examples/master_renderer_test"
cargo run --package pill_standalone --features rendering
```

Drag with the left mouse button, hold the arrow keys or A/D, or push the left
stick to turn the helmet. Scroll or push the right stick to move the camera
closer or farther. Gamepad A rumbles the pad.

## Assets

`res` holds the helmet mesh, its four PBR maps (base colour, normal, metallic-
roughness and emissive), their import metadata and the material that binds them
to the chain's shader. `AssetLoader::Path` resolves paths relative to this
project's `res` directory; the `.meta` files hold each asset's guid and import
settings. `build.rs` cooks the HLSL shader sources into WGSL beside them.

## Attribution

The helmet is a conversion of Khronos' glTF sample asset
[DamagedHelmet](https://github.com/KhronosGroup/glTF-Sample-Assets/tree/main/Models/DamagedHelmet).
See `res/textures/HELMET_LICENSE.md` for the full terms:

- © 2018, ctxwing - [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/legalcode)
- © 2016, theblueturtle_ - [CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/legalcode)

The second term is NonCommercial: these files are not covered by the
repository's MIT/Apache-2.0 licences and may not be used commercially.
