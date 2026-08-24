//! Advanced system parameter infrastructure that allows automatic parameter
//! resolution for system functions.
//!
//! # Responsibilities
//!
//! - Defines the [`SystemParam`] trait that lets function parameters extract
//!   themselves from the [`World`] (e.g., `Query<T>`, `Commands`, `Res<T>`).
//! - Provides [`IntoSystem`] to convert plain functions into boxed [`System`] trait objects.
//! - Implements the [`System`] trait for registered functions, enabling
//!   the engine to call them generically.
//!
//! # Design
//!
//! System functions are layered into four cooperating traits:
//!
//! - [`SystemParam`] extracts each argument from the [`World`] and
//!   [`CommandQueue`] at call time and reports its access pattern to the
//!   scheduler for dependency analysis.
//! - [`SystemParamFunction`] adapts a plain `FnMut` closure with `SystemParam`
//!   arguments into a uniform runnable form.
//! - [`IntoSystem`] boxes that adapted function into a [`System`] trait object
//!   the engine can store and invoke generically.
//! - [`SystemOutput`] normalizes the function's return value (`()` or
//!   `Result<(), SystemError>`) into a single error-reporting result.
//!
//! The [`System`] trait is the uniform invocation surface the engine drives;
//! everything else exists to convert user functions into that surface.
//!
//! SAFETY: Warning: Lifetime Transmutation
//!
//! This module uses `std::mem::transmute` to convert references with actual
//! lifetimes to `'static` lifetimes. This is a deliberate design choice to
//! work around Rust's borrowing rules in the context of dynamic system execution.
//!
//! This is technically undefined behavior if the reference escapes the system.
//!
//! The safety of this pattern relies on these invariants being upheld:
//!
//! 1. System parameters must NOT be stored - Parameters like `Query<'static, Q>`
//!    and `Commands<'static>` must only be used within the system function body.
//!    Storing them in static variables, global state, or any location that outlives
//!    the system call is undefined behavior.
//!
//! 2. System parameters must NOT escape via closures - Do not capture system
//!    parameters in closures that outlive the system (e.g., background threads, async
//!    tasks, or callbacks registered for later execution).
//!
//! 3. System parameters must NOT be returned - While the type system prevents
//!    most cases, do not use unsafe code to extract and return the inner reference.
//!
//! ## Why This Pattern?
//!
//! Rust's type system cannot express "this reference lives exactly as long as
//! this function call" without GATs (Generic Associated Types) and complex
//! lifetime machinery. The `'static` transmutation is a pragmatic solution
//! used by many ECS frameworks including early versions of Bevy.
//!
//! ## Safe Alternatives (Not Yet Implemented)
//!
//! - Token type pattern: Pass a lifetime-bound token that proves the borrow
//!   is still valid, making misuse a compile error.
//! - GAT-based SystemParam: Use Generic Associated Types to properly model
//!   the lifetime relationship (requires more complex trait bounds).
//!
//! ## Example: Safe Usage
//!
//! ```no_run
//! # use pill_engine::*;
//! # #[derive(Debug, Clone)] struct Position { x: f32, y: f32 }
//! # impl Component for Position {}
//! # #[derive(Debug, Clone)] struct Velocity { x: f32, y: f32 }
//! # impl Component for Velocity {}
//! fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
//!     for (mut pos, vel) in query.iter_mut() {
//!         pos.x += vel.x;  // OK - using within system
//!     }
//! }  // query dropped here - lifetime ends safely
//! ```
//!
//! ## Example: UNSAFE Usage (DO NOT DO THIS)
//!
//! ```ignore
//! static mut LEAKED_QUERY: Option<Query<'static, &Position>> = None;
//!
//! fn bad_system(query: Query<&Position>) {
//!     unsafe { LEAKED_QUERY = Some(query); }  // UNDEFINED BEHAVIOR!
//! }
//! ```

// Current crate
use crate::commands::{CommandQueue, Commands};
use crate::error::SystemError;
use crate::query::{Query, QueryFilter, QueryTarget, Res, ResMut};
use crate::resource::{Resource, ResourceId};
use crate::scheduler::SystemAccess;
use crate::world::World;

// =============================================================================
// System
// =============================================================================

/// Trait for systems that can be executed by the Engine
///
/// Systems are functions that operate on World data. They are executed
/// every frame and can read/write components, create entities, etc.
///
/// A system reports its own failure through [`Result::Err`]; the engine
/// records it and continues with the remaining systems.
///
/// Must be Send to support parallel execution.
pub trait System: Send {
    /// Executes one system invocation, returning its failure when it aborted.
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue) -> Result<(), SystemError>;
}

/// Accepted return types of a system function.
///
/// Systems may return `()` when they cannot fail or `Result<(), SystemError>`
/// when they can. The engine converts both into the uniform result type used
/// by the execution loops.
pub trait SystemOutput: 'static {
    /// Converts one system return value into the uniform result type.
    fn into_system_result(self) -> Result<(), SystemError>;
}

impl SystemOutput for () {
    fn into_system_result(self) -> Result<(), SystemError> {
        Ok(())
    }
}

impl SystemOutput for Result<(), SystemError> {
    fn into_system_result(self) -> Result<(), SystemError> {
        self
    }
}

/// Implement System for any FnMut closure with the right signature
///
/// This allows us to store system closures in a `Vec<Box<dyn System>>`.
impl<F, Output> System for F
where
    F: FnMut(&mut World, &mut CommandQueue) -> Output + Send,
    Output: SystemOutput,
{
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue) -> Result<(), SystemError> {
        self(world, queue).into_system_result()
    }
}

// =============================================================================
// SystemParam
// =============================================================================

/// SystemParam trait - any type that can be extracted as a system parameter
///
/// This is the core of the flexible system architecture. Types that implement
/// SystemParam can be used as function parameters in systems.
///
/// SAFETY:
///
/// Implementations use lifetime transmutation internally. See module-level docs
/// for the safety invariants that must be upheld.
///
/// The returned parameter must not escape the system function scope.
pub trait SystemParam: Sized {
    /// Fetches the parameter from world state.
    ///
    /// SAFETY: Contract
    ///
    /// The returned value has a `'static` lifetime marker but actually borrows
    /// from `world` and `queue`. Callers must ensure:
    ///
    /// 1. The returned value is dropped before the system function returns
    /// 2. The returned value is not stored in static/global state
    /// 3. The returned value is not moved into background threads or async tasks
    ///
    /// Violating these invariants is undefined behavior.
    fn fetch(world: &mut World, queue: &mut CommandQueue) -> Self;

    /// Reports the component access pattern for dependency analysis.
    ///
    /// Called during system registration to build the execution graph. The
    /// default implementation reports no access (for things like `State`).
    fn report_access(_access: &mut SystemAccess) {
        // Default: no component access
    }
}

/// Commands is a SystemParam - provides deferred entity operations
impl SystemParam for Commands<'static> {
    fn fetch(world: &mut World, queue: &mut CommandQueue) -> Self {
        // CRITICAL RISK
        // SAFETY: Lifetime transmutation from actual borrow to 'static.
        //
        // This is sound IFF the caller upholds the SystemParam safety contract:
        // - The Commands<'static> must not escape the system function
        // - The Commands<'static> must not be stored in global/static state
        // - The Commands<'static> must be dropped before system returns
        //
        // The Engine's system execution infrastructure ensures these invariants
        // by calling systems as opaque functions that cannot return the parameter.
        //
        // Undefined behavior if Commands escapes (e.g., stored in static variable,
        // moved to another thread, or captured in an escaping closure).
        unsafe { std::mem::transmute(Commands::new(queue, world)) }
    }

    fn report_access(access: &mut SystemAccess) {
        // Commands can create/destroy entities and add/remove components
        // This requires exclusive World access
        access.set_uses_commands(true);
    }
}

/// Generic Query is a SystemParam - works for ANY WorldQuery type
///
/// This implementation allows any query pattern to be used as a system parameter
/// without needing separate implementations for each query type.
impl<Q: QueryTarget + 'static, F: QueryFilter + 'static> SystemParam for Query<'static, Q, F> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue) -> Self {
        // SAFETY: Lifetime transmutation from actual borrow to 'static.
        //
        // This is sound IFF the caller upholds the SystemParam safety contract:
        // - The Query<'static, Q, F> must not escape the system function
        // - The Query<'static, Q, F> must not be stored in global/static state
        // - The Query<'static, Q, F> must be dropped before system returns
        //
        // The Engine's system execution infrastructure ensures these invariants
        // by calling systems as opaque functions that cannot return the parameter.
        //
        // UNDEFINED BEHAVIOR if Query escapes (e.g., stored in static variable,
        // moved to another thread, or captured in an escaping closure).
        unsafe {
            let query: Query<Q, F> = Query::new(world);
            std::mem::transmute(query)
        }
    }

    fn report_access(access: &mut SystemAccess) {
        let (reads, writes) = Q::report_component_access();
        for comp_id in reads {
            access.add_read(comp_id);
        }
        for comp_id in writes {
            access.add_write(comp_id);
        }
        // Filters that gate on a component (e.g. Changed<T>, With<T>) need
        // read access to that component's storage so they don't conflict
        // with concurrent writers of the same component in another system.
        for comp_id in F::included_component_ids() {
            access.add_read(comp_id);
        }
    }
}

/// Res<T> is a SystemParam - provides immutable access to a Resource
///
/// The scheduler tracks this as a resource read, allowing multiple systems
/// to read the same resource in parallel.
impl<T: Resource> SystemParam for Res<'static, T> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue) -> Self {
        // SAFETY: Lifetime transmutation from actual borrow to 'static.
        //
        // This is sound IFF the caller upholds the SystemParam safety contract:
        // - The Res<'static, T> must not escape the system function
        // - The Res<'static, T> must not be stored in global/static state
        // - The Res<'static, T> must be dropped before system returns
        //
        // UNDEFINED BEHAVIOR if Res escapes.
        unsafe {
            let res: Res<T> = Res::new(&*world);
            std::mem::transmute(res)
        }
    }

    fn report_access(access: &mut SystemAccess) {
        access.add_resource_read(ResourceId::of::<T>());
    }
}

/// ResMut<T> is a SystemParam - provides mutable access to a Resource
///
/// The scheduler tracks this as a resource write, preventing other systems
/// from accessing the same resource in parallel.
impl<T: Resource> SystemParam for ResMut<'static, T> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue) -> Self {
        // SAFETY: Lifetime transmutation from actual borrow to 'static.
        //
        // This is sound IFF the caller upholds the SystemParam safety contract:
        // - The ResMut<'static, T> must not escape the system function
        // - The ResMut<'static, T> must not be stored in global/static state
        // - The ResMut<'static, T> must be dropped before system returns
        //
        // UNDEFINED BEHAVIOR if ResMut escapes.
        unsafe {
            let res: ResMut<T> = ResMut::new(world);
            std::mem::transmute(res)
        }
    }

    fn report_access(access: &mut SystemAccess) {
        access.add_resource_write(ResourceId::of::<T>());
    }
}

// =============================================================================
// SystemParam Tuple Implementations
// =============================================================================

/// Macro to implement SystemParam for tuples
///
/// This allows systems to take multiple parameters. Each parameter is
/// fetched independently and combined into a tuple.
macro_rules! impl_system_param_tuple {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<$($T: SystemParam),*> SystemParam for ($($T,)*) {
            fn fetch(world: &mut World, queue: &mut CommandQueue) -> Self {
                ($($T::fetch(world, queue),)*)
            }

            fn report_access(access: &mut SystemAccess) {
                $($T::report_access(access);)*
            }
        }
    };
}

// Implement for tuples of different sizes (0 to 6 parameters)
impl SystemParam for () {
    fn fetch(_world: &mut World, _queue: &mut CommandQueue) -> Self {}

    fn report_access(_access: &mut SystemAccess) {
        // Empty tuple has no access
    }
}

impl_system_param_tuple!(A);
impl_system_param_tuple!(A, B);
impl_system_param_tuple!(A, B, C);
impl_system_param_tuple!(A, B, C, D);
impl_system_param_tuple!(A, B, C, D, E);
impl_system_param_tuple!(A, B, C, D, E, F1);

// =============================================================================
// SystemParamFunction
// =============================================================================

/// SystemParamFunction trait - functions that can be converted to systems
///
/// This trait is implemented for functions with different numbers of
/// SystemParam parameters. It provides the bridge between user-written
/// functions and the System trait. The associated [`Output`] is the
/// function's return type, either `()` or `Result<(), SystemError>`.
///
/// [`Output`]: SystemParamFunction::Output
pub trait SystemParamFunction<Input: SystemParam>: 'static {
    /// The function's return value, `()` or `Result<(), SystemError>`.
    type Output: SystemOutput;

    /// Runs the wrapped function with the resolved parameters.
    ///
    /// The `input` tuple carries the parameters fetched from the [`World`] and
    /// [`CommandQueue`]; the return value is normalized by [`SystemOutput`].
    fn run(&mut self, input: Input) -> Self::Output;
}

/// Macro to implement SystemParamFunction for functions with different arities
macro_rules! impl_system_param_function {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<F, Output, $($T: SystemParam),*> SystemParamFunction<($($T,)*)> for F
        where
            F: FnMut($($T),*) -> Output + Send + 'static,
            Output: SystemOutput,
        {
            type Output = Output;

            fn run(&mut self, input: ($($T,)*)) -> Self::Output {
                let ($($T,)*) = input;
                self($($T),*)
            }
        }
    };
}

// Implement for functions with 0 parameters
impl<F, Output> SystemParamFunction<()> for F
where
    F: FnMut() -> Output + Send + 'static,
    Output: SystemOutput,
{
    type Output = Output;

    fn run(&mut self, _input: ()) -> Self::Output {
        self()
    }
}

// Implement for functions with 1-6 parameters
impl_system_param_function!(A);
impl_system_param_function!(A, B);
impl_system_param_function!(A, B, C);
impl_system_param_function!(A, B, C, D);
impl_system_param_function!(A, B, C, D, E);
impl_system_param_function!(A, B, C, D, E, F1);

// =============================================================================
// IntoSystem
// =============================================================================

/// Trait for converting functions into Systems
///
/// This uses the SystemParam infrastructure to automatically resolve parameters.
/// When you call engine.register_system(name, function), this trait handles
/// the conversion from a plain function to a boxed System trait object.
pub trait IntoSystem<Input: SystemParam> {
    /// Boxes this function into a [`System`] trait object the engine can store
    /// and invoke generically.
    fn into_system(self) -> Box<dyn System>;

    /// Boxes this function so its implementation can be replaced at runtime.
    ///
    /// Identical to [`Self::into_system`] except that the call to the user's
    /// function goes through `slot`. Replacing the implementation is then a
    /// single atomic store: the returned box, its vtable, the scheduler graph
    /// and the system's metadata all stay put, and the `World` is untouched.
    ///
    /// The slot is initialized here because this is the only place the concrete
    /// `F` and `Input` are known.
    #[cfg(feature = "hot_patch")]
    fn into_hot_system(self, slot: std::sync::Arc<crate::hot_patch::HotSlot>) -> Box<dyn System>;
}

/// Implement IntoSystem for any function that implements SystemParamFunction
///
/// This is the magic that makes everything work together:
/// 1. Function has parameters that implement SystemParam
/// 2. Those parameters are extracted via SystemParam::fetch
/// 3. The function is called with those parameters
/// 4. All wrapped in a System trait object for storage in the Engine
impl<F, Input> IntoSystem<Input> for F
where
    F: SystemParamFunction<Input> + Send + 'static,
    Input: SystemParam,
{
    fn into_system(mut self) -> Box<dyn System> {
        Box::new(move |world: &mut World, queue: &mut CommandQueue| {
            // Resolve the system's parameters from the world and queue.
            let input = Input::fetch(world, queue);
            // Invoke the wrapped function and normalize its return value.
            self.run(input).into_system_result()
        })
    }

    #[cfg(feature = "hot_patch")]
    fn into_hot_system(mut self, slot: std::sync::Arc<crate::hot_patch::HotSlot>) -> Box<dyn System> {
        // The baseline implementation and the signature to gate replacements
        // against. This is the only place the concrete `F` and `Input` exist.
        slot.initialize(
            <F as SystemParamFunction<Input>>::run as *const () as usize,
            crate::hot_patch::signature_hash::<F, Input>(),
        );

        Box::new(move |world: &mut World, queue: &mut CommandQueue| {
            // Resolve the system's parameters exactly as the non-patchable path
            // does, so a hot system and a plain one behave identically.
            let input = Input::fetch(world, queue);

            // One acquire load from a hot cache line, then an indirect call.
            // Measured at under 0.2 ns/call against a direct call, which is
            // below the noise floor of ABI and register-allocation accidents.
            let address = slot.current();

            // SAFETY: `address` is either the baseline written by `initialize`
            // above - `<F as SystemParamFunction<Input>>::run`, exactly this
            // type - or a replacement accepted by `HotSlot::install`, which
            // refuses any address whose signature hash differs from this
            // monomorphization's. A replacement lives in a patch library the
            // host never unloads, so the address stays executable for the
            // process lifetime. The acquire load pairs with install's release
            // store, so the replacement's code is visible before it is called.
            let run: fn(&mut F, Input) -> F::Output =
                unsafe { std::mem::transmute::<usize, fn(&mut F, Input) -> F::Output>(address) };

            run(&mut self, input).into_system_result()
        })
    }
}
