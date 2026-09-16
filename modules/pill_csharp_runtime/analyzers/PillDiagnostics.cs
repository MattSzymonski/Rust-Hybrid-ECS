// Diagnostic descriptors for the gameplay scripting rules.
//
// Responsibilities:
// - Declare every PILLxxxx rule in one place, with the message a script author
//   reads and the reason the rule exists.
//
// Design:
// - The ranges match the documented ID space: 01xx system declaration shape,
//   02xx frame and thread scope, 03xx static state and reload unloadability,
//   04xx component struct layout.
// - Correctness hazards are Error rather than Warning. A warning in a gameplay
//   project is a warning nobody reads, and every rule here describes something
//   that either cannot work or corrupts memory. The one exception is PILL0301,
//   which stays a warning until managed systems can reach engine resources and
//   therefore have somewhere legal to keep per-frame state.

using Microsoft.CodeAnalysis;

namespace PillScriptAnalyzers
{
    /// <summary>Every rule the gameplay analyzer reports.</summary>
    internal static class PillDiagnostics
    {
        private const string DeclarationCategory = "PillSystemDeclaration";
        private const string ScopeCategory = "PillFrameScope";
        private const string StateCategory = "PillStaticState";
        private const string LayoutCategory = "PillComponentLayout";

        /// <summary>Build one descriptor; every rule is enabled by default.</summary>
        private static DiagnosticDescriptor Rule(
            string id,
            string title,
            string messageFormat,
            string category,
            DiagnosticSeverity severity,
            string description) =>
            new DiagnosticDescriptor(
                id, title, messageFormat, category, severity,
                isEnabledByDefault: true, description: description);

        // ------------------------------------------------------------------
        // 01xx - system declaration shape
        // ------------------------------------------------------------------

        internal static readonly DiagnosticDescriptor MustBeStatic = Rule(
            "PILL0101",
            "An ECS system must be static",
            "'{0}' carries [{1}] but is an instance method, so it is never discovered and will never run",
            DeclarationCategory,
            DiagnosticSeverity.Error,
            "Discovery reflects over static methods only. An attribute on an instance method is " +
            "not rejected, it is invisible: the system silently never runs. Make the method static.");

        internal static readonly DiagnosticDescriptor MustReturnVoid = Rule(
            "PILL0103",
            "An ECS system must return void",
            "'{0}' returns '{1}'; an ECS system must return void",
            DeclarationCategory,
            DiagnosticSeverity.Error,
            "The scheduler invokes a system as a parameterless action and has nowhere to put a " +
            "return value.");

        internal static readonly DiagnosticDescriptor ParameterBudget = Rule(
            "PILL0102",
            "An ECS system has an unsupported parameter list",
            "'{0}' declares {1} parameters; an ECS system must declare between 1 and {2}, each a query or Commands",
            DeclarationCategory,
            DiagnosticSeverity.Error,
            "The budget matches the native system parameter arity, so a system ported between " +
            "Rust and C# can keep the same signature.");

        // ------------------------------------------------------------------
        // 02xx - frame and thread scope
        // ------------------------------------------------------------------

        internal static readonly DiagnosticDescriptor NoAsyncSystem = Rule(
            "PILL0201",
            "An ECS system may not be async",
            "'{0}' is async; an ECS system runs inside a scheduled scope that ends when it returns",
            ScopeCategory,
            DiagnosticSeverity.Error,
            "An await suspends the method, the scope is torn down, and the continuation would " +
            "resume with no world and possibly a stale chunk pointer. For work that spans " +
            "frames, keep state in a component and advance it each frame.");

        // ------------------------------------------------------------------
        // 03xx - static state and reload unloadability
        // ------------------------------------------------------------------

        internal static readonly DiagnosticDescriptor MutableStaticState = Rule(
            "PILL0301",
            "Mutable static state in a type that declares ECS systems",
            "'{0}' is mutable static state in a type that declares ECS systems",
            StateCategory,
            DiagnosticSeverity.Warning,
            "Systems with disjoint component access run on different threads in the same frame. " +
            "The scheduler derives that from component access and cannot see a static, so two " +
            "systems sharing one race. Statics also reset on every hot reload, because each " +
            "reload loads the assembly into a fresh collectible context. This is a warning " +
            "rather than an error until managed systems can reach engine resources, which is " +
            "where per-frame state belongs.");

        internal static readonly DiagnosticDescriptor StaticEventBlocksUnload = Rule(
            "PILL0302",
            "A static event prevents the project assembly from unloading",
            "'{0}' is a static event; a subscriber roots the collectible load context and every reload then leaks an assembly",
            StateCategory,
            DiagnosticSeverity.Error,
            "Assembly unloading is cooperative: it completes only when nothing outside the " +
            "context references it. A static event is one of the documented blockers.");

        internal static readonly DiagnosticDescriptor BackgroundWorkBlocksUnload = Rule(
            "PILL0303",
            "Background work prevents unloading and runs outside any frame",
            "'{0}' starts background work; its callback runs outside any scheduled system and it roots the collectible load context",
            StateCategory,
            DiagnosticSeverity.Error,
            "A thread with frames from the assembly prevents unload, and the ECS API is valid " +
            "only on the thread the scheduler called you on, and only before you return.");

        internal static readonly DiagnosticDescriptor GcHandleBlocksUnload = Rule(
            "PILL0304",
            "A GC handle prevents the project assembly from unloading",
            "GCHandle.Alloc roots the collectible load context; every reload then retains an assembly until the host restarts",
            StateCategory,
            DiagnosticSeverity.Error,
            "Strong and pinned GC handles are documented blockers of collectible assembly " +
            "unloading, from inside the context as well as outside it.");

        internal static readonly DiagnosticDescriptor FinalizerDelaysUnload = Rule(
            "PILL0305",
            "A finalizer delays assembly unloading",
            "'{0}' declares a finalizer, which adds collection passes before the retiring assembly can unload",
            StateCategory,
            DiagnosticSeverity.Warning,
            "Finalizable objects extend how many collections an unload needs. Nothing in a " +
            "gameplay project should own an unmanaged resource directly.");

        // ------------------------------------------------------------------
        // 04xx - component struct layout
        // ------------------------------------------------------------------

        internal static readonly DiagnosticDescriptor UnsupportedLayoutKind = Rule(
            "PILL0401",
            "Unsupported component layout",
            "'{0}' uses {1}; a component must be a sequential-layout struct, or an explicit-layout struct with a declared Size",
            LayoutCategory,
            DiagnosticSeverity.Error,
            "The host strides a native column by the size the manifest describes. Auto layout " +
            "has no describable order, and an explicit layout without a declared Size has no " +
            "describable stride.");

        internal static readonly DiagnosticDescriptor PackIsIgnored = Rule(
            "PILL0402",
            "StructLayout.Pack is not honoured for components",
            "'{0}' sets StructLayout.Pack, which the component manifest does not model; the declared size and the runtime size would disagree",
            LayoutCategory,
            DiagnosticSeverity.Error,
            "The manifest computes a component's layout with natural alignment. A packed struct " +
            "is smaller to the runtime than the column stride derived from the manifest, so the " +
            "two sizes disagree and the component is refused at registration.");

        internal static readonly DiagnosticDescriptor UnsupportedFieldType = Rule(
            "PILL0403",
            "Unsupported component field type",
            "'{0}' has type '{1}', which a component field may not have",
            LayoutCategory,
            DiagnosticSeverity.Error,
            "Component rows are copied as raw bytes and freed without running destructors, so " +
            "every field must be a blittable value with nothing to release. 'bool' and 'char' " +
            "are excluded separately because their marshalled sizes differ from their runtime " +
            "sizes; use byte and ushort.");
    }
}
