// ============================================================================
// System Infrastructure - Bevy-Style SystemParam
// ============================================================================
//! Advanced system parameter infrastructure that allows automatic parameter
//! resolution for system functions.
//!
//! This module implements a Bevy-style system parameter system where:
//! 1. Each parameter type implements SystemParam to extract itself from World
//! 2. Functions are automatically converted to systems based on their parameters
//! 3. No manual wrapper code needed - just write functions and register them

use crate::commands::{CommandQueue, Commands};
use crate::component::Component;
use crate::query::{GlobalComponentQuery, Query, WorldQuery};
use crate::world::World;

/// State that persists between system calls
///
/// This stores system-local data that needs to persist across frames.
/// Currently just used for the dead_report_system's timer.
pub struct SystemState {
    pub last_report_time: f32,
}

impl SystemState {
    pub fn new() -> Self {
        Self {
            last_report_time: 0.0,
        }
    }
}

/// Trait for systems that can be executed by the Engine
///
/// Systems are functions that operate on World data. They are executed
/// every frame and can read/write components, spawn entities, etc.
pub trait System {
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue, state: &mut SystemState);
}

/// Implement System for any FnMut closure with the right signature
///
/// This allows us to store system closures in a Vec<Box<dyn System>>.
impl<F> System for F
where
    F: FnMut(&mut World, &mut CommandQueue, &mut SystemState),
{
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue, state: &mut SystemState) {
        self(world, queue, state);
    }
}

// ============================================================================
// SystemParam - Automatic Parameter Extraction
// ============================================================================

/// SystemParam trait - any type that can be extracted as a system parameter
///
/// This is the core of the flexible system architecture. Types that implement
/// SystemParam can be used as function parameters in systems.
///
/// The trait uses lifetime transmutation internally to work around Rust's
/// borrowing rules while maintaining safety.
pub trait SystemParam: Sized {
    /// Fetch the parameter from world state
    ///
    /// SAFETY: The returned value has a 'static lifetime for technical reasons,
    /// but it actually only lives as long as the system execution.
    fn fetch(world: &mut World, queue: &mut CommandQueue, state: &mut SystemState) -> Self;
}

/// Commands is a SystemParam - provides deferred entity operations
impl SystemParam for Commands<'static> {
    fn fetch(_world: &mut World, queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        // SAFETY: The Commands will only live as long as the system execution.
        // We transmute the lifetime to 'static for technical reasons, but
        // the system infrastructure ensures it doesn't outlive its borrow.
        unsafe { std::mem::transmute(Commands::new(queue)) }
    }
}

/// Generic Query is a SystemParam - works for ANY WorldQuery type
///
/// This implementation allows any query pattern to be used as a system parameter
/// without needing separate implementations for each query type.
impl<Q: WorldQuery + 'static> SystemParam for Query<'static, Q> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        unsafe {
            // Create query with actual lifetime, then transmute to 'static
            // SAFETY: The query will only live as long as the system execution
            let query: Query<Q> = Query::new(world);
            std::mem::transmute(query)
        }
    }
}

/// Generic GlobalComponentQuery is a SystemParam - works for ANY Component type
///
/// This allows accessing any global/singleton component in systems.
impl<T: Component> SystemParam for GlobalComponentQuery<'static, T> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        unsafe {
            // Create query with actual lifetime, then transmute to 'static
            // SAFETY: The query will only live as long as the system execution
            let query: GlobalComponentQuery<T> = GlobalComponentQuery::new(world);
            std::mem::transmute(query)
        }
    }
}

/// State wrapper for accessing persistent system state
///
/// This allows systems to maintain state between frames (like timers).
pub struct State<T>(pub T);

impl SystemParam for State<&'static mut f32> {
    fn fetch(_world: &mut World, _queue: &mut CommandQueue, state: &mut SystemState) -> Self {
        // SAFETY: Transmuting lifetime for state access
        unsafe { State(std::mem::transmute(&mut state.last_report_time)) }
    }
}

// ============================================================================
// SystemParam Tuple Implementations
// ============================================================================

/// Macro to implement SystemParam for tuples
///
/// This allows systems to take multiple parameters. Each parameter is
/// fetched independently and combined into a tuple.
macro_rules! impl_system_param_tuple {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<$($T: SystemParam),*> SystemParam for ($($T,)*) {
            fn fetch(world: &mut World, queue: &mut CommandQueue, state: &mut SystemState) -> Self {
                ($($T::fetch(world, queue, state),)*)
            }
        }
    };
}

// Implement for tuples of different sizes (0 to 6 parameters)
impl SystemParam for () {
    fn fetch(_world: &mut World, _queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        ()
    }
}

impl_system_param_tuple!(A);
impl_system_param_tuple!(A, B);
impl_system_param_tuple!(A, B, C);
impl_system_param_tuple!(A, B, C, D);
impl_system_param_tuple!(A, B, C, D, E);
impl_system_param_tuple!(A, B, C, D, E, F1);

// ============================================================================
// SystemParamFunction - Function to System Conversion
// ============================================================================

/// SystemParamFunction trait - functions that can be converted to systems
///
/// This trait is implemented for functions with different numbers of
/// SystemParam parameters. It provides the bridge between user-written
/// functions and the System trait.
pub trait SystemParamFunction<Input: SystemParam>: 'static {
    fn run(&mut self, input: Input);
}

/// Macro to implement SystemParamFunction for functions with different arities
macro_rules! impl_system_param_function {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<F, $($T: SystemParam),*> SystemParamFunction<($($T,)*)> for F
        where
            F: FnMut($($T),*) + 'static,
        {
            fn run(&mut self, input: ($($T,)*)) {
                let ($($T,)*) = input;
                self($($T),*)
            }
        }
    };
}

// Implement for functions with 0 parameters
impl<F> SystemParamFunction<()> for F
where
    F: FnMut() + 'static,
{
    fn run(&mut self, _input: ()) {
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

// ============================================================================
// IntoSystem - Automatic System Conversion
// ============================================================================

/// Trait for converting functions into Systems
///
/// This uses the SystemParam infrastructure to automatically resolve parameters.
/// When you call engine.register_system(name, function), this trait handles
/// the conversion from a plain function to a boxed System trait object.
pub trait IntoSystem<Input: SystemParam> {
    fn into_system(self) -> Box<dyn System>;
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
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    fn into_system(mut self) -> Box<dyn System> {
        Box::new(
            move |world: &mut World, queue: &mut CommandQueue, state: &mut SystemState| {
                let input = Input::fetch(world, queue, state);
                self.run(input);
            },
        )
    }
}
