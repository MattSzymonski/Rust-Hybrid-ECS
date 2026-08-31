
Tool to track callstacks and exports them as trees




pill_standalone
- `modules\pill_host\src\runner.rs::run` (headless or windowed)
	-    windowed - Creates a windowed application which sets up the project, creates hidden window and attaches renderer to that hidden window, renders one frame and shows the window
	-    headless - sets up the project, simply starts loop that calls `run_one_frame`
	- `modules\pill_host\src\runtime.rs::setup` - Setup host
		- Create an engine
		- Create engine api
		- Create optional modules and hot reload source watchers for them
		- Build and load project module. If it is csharp then generate all bindings with codegen


- `modules\pill_host\src\runtime.rs::run_one_frame` 
	- check if extension modules source code did not change
	- if changed then reload them
	- reload csharp???
	- Call engine.process_frame
	- Gather errors and calls hosts to show them `host.report_frame_error(summary);`
	- Invoke the native compatibility update????






```
// An extension module the project links directly is compiled two times -> into:
// - the project DLL code itself (emvedded)
// - the extension module independent DLL aritfact
// So after the module swaps, the project still runs the old embedded copy of that crate. <- ????? WHAT? it should have the newly embedded
```




ModuleExposedComponent
ProjectModuleBackend
ProjectModuleConfig


## Modules
- pill_standalone - creates host and calls it to start loop
- pill_host::runner - creates host object and window and loop that calls `run_one_frame`
- pill_host::runtime - defines actual host struct and logic, has run_one_frame functions with calls engine.process_frame
- pill_host::telemetry - has just `init_telemetry` function which inits telemetry and returns `TelemetryHandles`
- pill_host::config - defines structs that store info about projects (rust and csharp) and the extension modules, like: paths, dotnet runtime, etc.


## Questions and actions

- [ ] Upgrade timeline on landing page
- [ ] Do even C# projects work?
- [ ] Does C# have function live patching
- [ ] Does C# have its own managed memory? When it declares some variables/consts, will they survive the hot reload?
- [ ] Does pill_host always include hot reloading logic which is needed only for dev builds???
- [ ] Measure the build size
- [ ] How much boilerplate project source code needs to have to work with the engine?
- [ ] Try to extract this renderer first.
- [ ] Modules lazy load???
- [ ] WASM
- [ ] Editor API? Editor loading time!
- [ ] How serialization work?
- [ ] How 
- [ ] When extension module is getting reloaded will other modules that depend on it also get reloaded even if they do not use any functions/structs from this extension. Even if they use some, functions. Can we just check if the ones that changed are used in the module and only then if yes recompile it? (Is in such module to module dependency, the embedding used or it is done via dynlib sharing?)

#### Lower pro
- [ ] Will the standalone run on linux? (as there is `#[cfg(windows)] pill_build_support::stage_std_dylib();`)
- [ ] Move pill_build_support to pill_core as it only have `stage_std_dylib` function and nothing else.
- [ ] Make pill_wgpu_renderer crate (what about rendering flag and window creation? pill_window crate imported by every renderer implementation?). 
	- [ ] Remove rendering flag from pill_host. Is it possible?
	- [ ] `modules\pill_host\src\runner.rs` has billion of `#[cfg(feature = "rendering")]` which is super messy.
- [ ] How to create WASM target? (as it is not a pill_standalone, right?)
- [ ] How this double library export work?
- [ ] Fix comments style in `modules\pill_host\src\lib.rs`
- [ ] Rename cs to csharp
- [ ] Rename "optional" module to "extension_module"
- [ ] Rename "watcher" to "hot_reload_watcher"
- [ ] Rename `.with_title("ECS Standalone Host")`, take name from HostConfig
	- [ ] Actually take it from game file `project_settings.yaml`
- [ ] Fix to many comment replication in `modules\pill_host\src\runner.rs`
- [ ] Make `engine.set_parallel_execution(true);` not hardcoded like this. It should come from project settings
- [ ] `let module_generation = Arc::new(AtomicU64::new(0));` why this is Arc and atomic?
- [ ] Rename `spawn_source_watcher` to `spawn_source_hot_reload_watcher`
- [ ] Why this is `OptionalModuleSlot::start` instead of `new`?
- [ ] If we are creating these hot reload source watchers and extension modules in host, then is the release build also using them and is composed out of modules instead of being statically compiled?
- [ ] Does OptionalModuleSlot::start() loads the dll?
- [ ] Why there is naming `LoadedProject::start` and `OptionalModuleSlot::start` (and not `ExtensionModule::start()`) Maybe because thay can be reloaded? but project also can be reloaded. And why it is called `LoadedProject` and not `ProjectModuleSlot`
- [ ] Make `"[host] Entering project loop. Edit {}/**/* to hot-reload.",` verbose and why it is not using our logging framework? Too early?
- [ ] Rename variables of `Host` to more clear
- [ ] `Step 1: Process a pending hot reload before running sys`, rework this comment, it is not understandable at all
- [ ] What if I reload extension module while engine is reloading some other one already?
- [ ] `host.loaded_project.poll_managed_reload();` is this for C# only? Why do we need assembly watchers
- [ ] Rename run_one_frame to process_frame
- [ ] `report_frame_error` runs every frame and can't be disabled. How heavy it is and does it exist in release builds as well?
- [ ] Rename all occurances of `game(s)` to `project(s)`
- [ ] Lines 456 to 481 in `modules\pill_host\src\runtime.rs`, i dont understand at all this pipeline
- [ ] Does editor use host as well?
- [ ] `project_settings.yaml`
	- [ ] Move `DEV_LOG_TARGET` to `project_settings.yaml`
		- [ ] Find other variables like that and move them as well
		- [ ] !!! Do a pass to find all the magic values in all the files
	- [ ] Write down what flag is doing what. There are `#[cfg(feature = "profiling")]`, `#[cfg(feature = "profiling-fine")]`, `#[cfg(feature = "metrics")]` all impacting `pub fn init_telemetry()` , `profiling-verify`. Can we have a feature (flag) nested inside other feature (flag)?
	- [ ] Move such things to`LevelFilter::DEBUG` and `telemetry_target::RENDERING` as well
- [ ] Rename non-full names like `let mut builder = TelemetryBuilder::new();` to full names, so `telemetry_builder`
- [ ] Consider moving pill_host/telemetry.rs to pill_core as it maks no sense for it to be in host (or maybe required dependency crates are already in pill_host and are also used by something else there so it is more optimal to keep it there???), there are even things like `pill_core::telemetry::telemetry_target::HOT_RELOAD` defined in pill_core already...
- [ ] Do watchers occupy whole treads of how does it work? What if we have 20 modules? each will get its own watcher and occupy the thread instead of having just one that will check source folders of all of them?
- [ ] Move constants like `MAX_GRAVEYARD_GENERATIONS` so a single `constants.rs` file instead of having them scattered among multiple `.rs` source files. (also `NO_COLOR`, `PILL_ANSI`)
- [ ] How C# behaves in the release builds? Does it require csharp runtime or it is somehow copiled to lower language (interpretable/rust/assembly?)
- [ ] Will hot reload work fine when we edit and save both definition change and function change? Will it be handled correctly?
- [ ] Isn't `project_module.rs` very similar to `optional_module.rs`? Maybe some of their code can be merged together? and what is `native_library.rs` in this context?
- [ ] Find and flag all windows only logic. Like possibly here `fn ansi_enabled() -> bool {` and make sure it handles other systems as well.
- [ ] Explain better what is `fn enable_windows_vt()`
- [ ] Is `console.rs` used anywhere except pill_host? Maybe it can be moved to `pill_core`?
- [ ] `const CSHARP_TARGET_FRAMEWORK: &str = "net8.0";` what does it mean? what are actually these versions? what they change? can we use other?
- [ ] How does this influence the build: `const HOST_CONFIG_FILE: &str = "pill_config.yaml";` What can this file define exactly? Can have a schema hardcoded in the engine and validate it to check if all the fields are always present there?
- [ ] Are any environment variables supported? or all is derived from project folder? 




 

    
How the Abi works and engine api
    


## What is?
- [ ] FrontendError::WindowCreation
- [ ] HostConfig::from_environment()
- [ ] ECS_LOG_DIR
- [ ] install_engine_report_handler
- [ ] init_telemetry
- [ ] error!(
- [ ] EngineError
- [ ] WindowEvent::RedrawRequested (when this is called? simply every frame?)
- [ ] "The engine clears each frame to black" - where?
- [ ] print_frame_statistics
- [ ] `crate::setup`
- [ ] ActiveEventLoop vs ControlFlow vs EventLoop
- [ ] event_loop.set_control_flow
- [ ] pill_engine::SystemOwner::optional_module(index),
- [ ] analytics::record_host_memory();
- [ ] analytics::print_startup_report();
- [ ] `reload_generation.fetch_add(1, Ordering::Release);` and `host.reload_generation.load(Ordering::Acquire)`
- [ ] `EngineMessage::to_plain_message`
- [ ] What is native and what is managed? What are "backends"  What are `host frontend` and `host backend`

```
    if matches!(
        module_config.backend,
        ProjectModuleBackend::NativeLibrary { .. }
    ) {
        cleanup_temporary_files(&workspace_root);
    }
```

## Facts
- Runner does the project setup and creates window after that. If anything fails (window creation, project creation, etc) it stores error that happened in itself and then does `event_loop.exit();`
- dawdawd


## What to check
- Where and how to implement and connect custom editor UI in extension modules?
- What about entity parenting?












- [ ] pill_host
	- [ ] src
		- [ ] csharp
			- [ ] abi.rs
			- [ ] backend.rs
			- [ ] codegen.rs
			- [ ] commands.rs
			- [ ] components.rs
			- [ ] context.rs
			- [ ] csharp_runtime.rs
			- [ ] mod.rs
			- [ ] queries.rs
			- [ ] tests.rs
		- [x] analytics.rs
		- [ ] build_runner.rs
		- [x] config.rs
		- [x] console.rs
		- [x] lib.rs
		- [x] native_library.rs
		- [x] optional_module.rs
		- [x] project_module.rs
		- [x] runner.rs 
		- [x] runtime.rs
		- [x] telemetry.rs
		- [x] watcher.rs
		- [x] Cargo.toml



