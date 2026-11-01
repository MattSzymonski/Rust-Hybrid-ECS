# pills_awakens

Two hundred thousand pills drifting through a curl noise field, among ancient
pillars, watched through a free flying camera.

The field is Bridson, Hourihan and Nordenstam's curl noise ("Curl Noise for
Procedural Fluid Flow", SIGGRAPH 2007): three decorrelated noise fields form a
vector potential, and each pill moves along its curl, which is divergence free -
so the pills swirl instead of bunching into sinks. A tuning overlay retunes the
field live, and the camera flies with WASD while a right-button drag looks
around.

`pills_awakens_rust` and `pills_awakens_csharp` run the same level, the same
system layout and the same tuning keys against the same `res` tree:

```
res/models/      the pill, the pillars and the ground
res/textures/    the maps the level's materials bind
res/materials/   one standalone material file per surface, naming its shader by guid
res/fonts/       the face the in-game overlay uses
res/ui/          the overlay's theme override
```

The maps are downscaled from the sources the level was authored with, so two
tracked copies of the same assets stay a reasonable size; the simulation itself
is unchanged.

## Run it

```powershell
cd modules
$env:PROJECT_PATH = "../examples/pills_awakens/pills_awakens_rust"
cargo run --package pill_standalone --features rendering
```

## Keys

| Keys                              | Does                      |
| --------------------------------- | ------------------------- |
| `W` `A` `S` `D`, `E`/`Q`, `Shift` | Fly, rise or sink, sprint |
| Right-drag mouse                  | Look around               |
| `T` / `G`                         | Narrow or widen the view  |
| `O` / `P`                         | Curl scale                |
| `L` / `K`                         | Attraction                |
| `M` / `N`                         | Damping                   |
| `V` / `B`                         | Curl epsilon              |
| `F3`                              | Debug draw                |

## What to try

- Change `PILL_COUNT` in `src/game.rs` (Rust) or `ProjectConstants.PillCount`
  (C#) and watch the reload rebuild the scene up to the new count.
- Tune the field with the keys above, then read the same values off the
  overlay; they are the resource the systems read.
