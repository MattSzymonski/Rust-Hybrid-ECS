// Roslyn source generator for the NativeAOT shipping posture.
//
// The reflection-based system discovery in `ProjectHost` cannot run under
// NativeAOT: `Activator.CreateInstance`, `Expression.Compile` and
// `MethodInfo.Invoke` all require dynamic code or reflection invokers that
// AOT does not provide. This generator instead emits, at compile time, a
// direct registration table for every `[EcsSystem]` / `[EcsStartup]` method:
// method: a static runner per system that constructs each query with `new`
// and calls the method directly (no reflection), plus the query descriptors
// needed for scheduler access derivation and the component manifest.
//
// The generated code is compiled into the *project* assembly (the only one
// that can see the gameplay types) and installed into `AotRegistry`
// (csharp_runtime) through a module initializer, so `ProjectHost` reads the
// same data shape it would have discovered reflectively.
//
// Only referenced when the project is published with PublishAot=true
// (see project_cs.csproj); ordinary JIT builds never run it.

using System;
using System.Collections.Generic;
using System.Collections.Immutable;
using System.Linq;
using System.Text;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;
using Microsoft.CodeAnalysis.Text;

namespace PillCSharpRuntimeMacros
{
    /// <summary>Emits the AOT system registry for a project assembly.</summary>
    [Generator(LanguageNames.CSharp)]
    public sealed class EcsAotRegistryGenerator : IIncrementalGenerator
    {
        private const string AotRegistryNamespace = "TracyLive.Loader";
        private const string AotRegistryTypeName = "AotRegistry";
        /// <summary>Parameter budget matching the runtime's managed system contract.</summary>
        private const int MaxSystemParameters = 6;
        // FullyQualifiedFormat renders with the `global::` prefix, so the
        // comparison strings must carry it too.
        private const string CommandsTypeName = "global::TracyLive.Commands";
        private const string IResourceParameterTypeName = "global::TracyLive.IResourceParameter";
        private const string ResTypeName = "global::TracyLive.Res<";
        private const string ResMutTypeName = "global::TracyLive.ResMut<";
        private const string IQueryDescriptorTypeName = "global::TracyLive.IQueryDescriptor";

        /// <summary>Fully-qualified symbol rendering with `global::` prefixes.</summary>
        private static readonly SymbolDisplayFormat FullyQualified =
            SymbolDisplayFormat.FullyQualifiedFormat;

        public void Initialize(IncrementalGeneratorInitializationContext context)
        {
            IncrementalValuesProvider<IMethodSymbol> attributedMethods = context
                .SyntaxProvider.CreateSyntaxProvider(
                    static (node, _) => IsAttributedMethod(node),
                    static (ctx, _) =>
                        (IMethodSymbol?)ctx.SemanticModel.GetDeclaredSymbol(ctx.Node))
                .Where(static method => method is not null)
                .Select(static (method, _) => method!);

            context.RegisterSourceOutput(
                attributedMethods.Collect(),
                static (sourceContext, methods) =>
                    Emit(sourceContext, methods));
        }

        /// <summary>Fast syntax filter: a static method carrying any attribute.</summary>
        private static bool IsAttributedMethod(SyntaxNode node)
        {
            if (node is not MethodDeclarationSyntax method)
                return false;
            if (method.AttributeLists.Count == 0)
                return false;
            // Static systems/startups are required by the runtime.
            return method.Modifiers.Any(SyntaxKind.StaticKeyword);
        }

        /// <summary>Emit the registry source into the compilation.</summary>
        private static void Emit(SourceProductionContext context, ImmutableArray<IMethodSymbol> methods)
        {
            List<IMethodSymbol> systems = new();
            List<IMethodSymbol> startups = new();
            foreach (IMethodSymbol method in methods)
            {
                foreach (AttributeData attribute in method.GetAttributes())
                {
                    string name = attribute.AttributeClass?.Name ?? "";
                    if (name == "EcsSystemAttribute")
                    {
                        systems.Add(method);
                        break;
                    }
                    if (name == "EcsStartupAttribute")
                    {
                        startups.Add(method);
                        break;
                    }
                }
            }

            // Deterministic order, matching the reflection path: by declaring
            // type full name, then method name.
            Comparison<IMethodSymbol> order = static (left, right) =>
            {
                int byType = string.CompareOrdinal(
                    left.ContainingType.ToDisplayString(FullyQualified),
                    right.ContainingType.ToDisplayString(FullyQualified));
                return byType != 0 ? byType : string.CompareOrdinal(left.Name, right.Name);
            };
            systems.Sort(order);
            startups.Sort(order);

            StringBuilder source = new StringBuilder();
            source.AppendLine("// <auto-generated> by PillCSharpRuntimeMacros.EcsAotRegistryGenerator");
            source.AppendLine("// Direct, reflection-free registrations of every [EcsSystem]/[EcsStartup]");
            source.AppendLine("// method, for the NativeAOT shipping posture. Do not edit.</auto-generated>");
            source.AppendLine($"namespace {AotRegistryNamespace}");
            source.AppendLine("{");
            source.AppendLine("    internal static class GeneratedAotRegistry");
            source.AppendLine("    {");

            List<string> systemEntries = new();
            List<string> startupEntries = new();

            for (int index = 0; index < systems.Count; index++)
            {
                IMethodSymbol method = systems[index];
                string receiver = method.ContainingType.ToDisplayString(FullyQualified);
                string entry = EmitSystem(source, method, receiver, index, systemEntries);
                if (entry is null)
                    context.ReportDiagnostic(Diagnostic.Create(
                        new DiagnosticDescriptor(
                            "PCS0001",
                            "Unsupported EcsSystem signature",
                            "System {0} has an unsupported signature for the AOT registry " +
                            "(expected up to six parameters: query parameters plus at most one Commands)",
                            "PillCSharpRuntimeMacros",
                            DiagnosticSeverity.Error,
                            isEnabledByDefault: true),
                        method.Locations.FirstOrDefault(),
                        method.ToDisplayString(FullyQualified)));
            }

            for (int index = 0; index < startups.Count; index++)
            {
                IMethodSymbol method = startups[index];
                string receiver = method.ContainingType.ToDisplayString(FullyQualified);
                EmitStartup(source, method, receiver, index, startupEntries);
            }

            // The registry arrays ProjectHost's AOT branch reads at startup.
            source.AppendLine(
                "        internal static readonly global::TracyLive.Loader.AotSystemRegistration[] Systems =");
            source.AppendLine("            new global::TracyLive.Loader.AotSystemRegistration[]");
            source.AppendLine("            {");
            foreach (string entry in systemEntries)
                source.AppendLine(entry + ",");
            source.AppendLine("            };");
            source.AppendLine();
            source.AppendLine(
                "        internal static readonly global::TracyLive.Loader.AotStartupRegistration[] Startups =");
            source.AppendLine("            new global::TracyLive.Loader.AotStartupRegistration[]");
            source.AppendLine("            {");
            foreach (string entry in startupEntries)
                source.AppendLine(entry + ",");
            source.AppendLine("            };");

            // Install through a module initializer so the host never reflects.
            // The registration arrays are read by ProjectHost's AOT branch.
            source.AppendLine("    }");
            source.AppendLine();
            source.AppendLine("    internal static class AotModule");
            source.AppendLine("    {");
            source.AppendLine("        [global::System.Runtime.CompilerServices.ModuleInitializer]");
            source.AppendLine("        internal static void Install()");
            source.AppendLine("        {");
            source.AppendLine("            global::TracyLive.Loader.AotRegistry.Install(");
            source.AppendLine("                GeneratedAotRegistry.Systems,");
            source.AppendLine("                GeneratedAotRegistry.Startups,");
            // The project assembly (this compilation's root) is what backs the
            // component manifest; the merged AOT image keeps csharp_runtime's
            // types in their own assembly, so the manifest must not enumerate
            // the runtime assembly instead.
            source.AppendLine("                typeof(GeneratedAotRegistry).Assembly);");
            source.AppendLine("        }");
            source.AppendLine("    }");
            source.AppendLine("}");
            context.AddSource("GeneratedAotRegistry.g.cs", SourceText.From(source.ToString(), Encoding.UTF8));
        }

        /// <summary>Classify one system parameter as query or Commands.</summary>
        private static bool IsCommands(ITypeSymbol type) =>
            type.ToDisplayString(FullyQualified) == CommandsTypeName;

        /// <summary>
        /// Classify one parameter as a resource declaration, returning its
        /// access mode: 0 for <c>Res&lt;T&gt;</c>, 1 for <c>ResMut&lt;T&gt;</c>.
        /// </summary>
        /// <remarks>
        /// Matched on the open generic's display prefix rather than on a symbol
        /// comparison, because the generator has no reference to the runtime's
        /// closed types. The interface check is what keeps that prefix from
        /// matching an unrelated type someone happens to name the same way.
        /// </remarks>
        private static byte? ResourceMode(ITypeSymbol type)
        {
            bool declaresContract = false;
            foreach (INamedTypeSymbol implemented in type.AllInterfaces)
            {
                if (implemented.ToDisplayString(FullyQualified) == IResourceParameterTypeName)
                {
                    declaresContract = true;
                    break;
                }
            }
            if (!declaresContract)
                return null;
            string name = type.ToDisplayString(FullyQualified);
            if (name.StartsWith(ResMutTypeName, StringComparison.Ordinal))
                return 1;
            if (name.StartsWith(ResTypeName, StringComparison.Ordinal))
                return 0;
            return null;
        }

        /// <summary>Whether a type is assignable to the IQueryDescriptor contract.</summary>
        private static bool IsQueryType(ITypeSymbol type)
        {
            if (type.TypeKind == TypeKind.Interface &&
                type.ToDisplayString(FullyQualified) == IQueryDescriptorTypeName)
                return true;
            foreach (INamedTypeSymbol implemented in type.AllInterfaces)
            {
                if (implemented.ToDisplayString(FullyQualified) == IQueryDescriptorTypeName)
                    return true;
            }
            return false;
        }

        /// <summary>Emit one system's query holders, runner, and registry entry.</summary>
        private static string? EmitSystem(
            StringBuilder source,
            IMethodSymbol method,
            string receiver,
            int index,
            List<string> entries)
        {
            // Classify parameters: queries and at most one Commands, within
            // the same parameter budget the runtime enforces.
            if (method.Parameters.Length == 0 || method.Parameters.Length > MaxSystemParameters)
                return null;
            bool usesCommands = false;
            foreach (IParameterSymbol parameter in method.Parameters)
            {
                if (IsCommands(parameter.Type))
                {
                    if (usesCommands)
                        return null; // second Commands parameter: unsupported
                    usesCommands = true;
                    continue;
                }
                if (ResourceMode(parameter.Type) is not null)
                    continue;
                if (!IsQueryType(parameter.Type))
                    return null; // non-query, non-Commands parameter: unsupported
            }

            // One static query holder per query parameter, so the runner can
            // hand each of them to the method in declared order.
            string[] queryFields = new string[method.Parameters.Length];
            for (int parameterIndex = 0; parameterIndex < method.Parameters.Length; parameterIndex++)
            {
                IParameterSymbol parameter = method.Parameters[parameterIndex];
                if (IsCommands(parameter.Type) || ResourceMode(parameter.Type) is not null)
                    continue;
                string field = $"s_query_{index}_{parameterIndex}";
                string queryType = parameter.Type.ToDisplayString(FullyQualified);
                source.AppendLine($"        private static readonly {queryType} {field} = new {queryType}();");
                queryFields[parameterIndex] = field;
            }

            string runMethod = $"RunSystem_{index}";
            // Build the invocation argument list in declared parameter order,
            // alongside the descriptor and name arrays the runtime consumes.
            List<string> arguments = new();
            List<string> descriptors = new();
            List<string> queryNames = new();
            List<string> resourceAccesses = new();
            for (int parameterIndex = 0; parameterIndex < method.Parameters.Length; parameterIndex++)
            {
                IParameterSymbol parameter = method.Parameters[parameterIndex];
                if (IsCommands(parameter.Type))
                {
                    arguments.Add("default");
                    continue;
                }
                // Res<T> and ResMut<T> carry no state - every access re-asks
                // the host - so the default value is the whole parameter, and
                // only the declaration it implies has to be recorded.
                if (ResourceMode(parameter.Type) is byte mode)
                {
                    arguments.Add("default");
                    string resource = ((INamedTypeSymbol)parameter.Type)
                        .TypeArguments[0].ToDisplayString(FullyQualified);
                    resourceAccesses.Add(
                        $"global::TracyLive.Loader.ResourceAccessRegistration" +
                        $".Of<{resource}>((byte){mode})");
                    continue;
                }
                arguments.Add(queryFields[parameterIndex]);
                descriptors.Add($"{queryFields[parameterIndex]}.Descriptor");
                queryNames.Add($"\"{parameter.Name}\"");
            }
            string args = string.Join(", ", arguments);
            source.AppendLine(
                $"        private static void {runMethod}() => {receiver}.{method.Name}({args});");

            string name = $"{method.ContainingType.ToDisplayString(FullyQualified)}.{method.Name}";
            entries.Add(
                $"            new global::TracyLive.Loader.AotSystemRegistration(" +
                $"\"{name}\", new global::TracyLive.QueryDescriptor?[] {{ {string.Join(", ", descriptors)} }}, " +
                $"new string[] {{ {string.Join(", ", queryNames)} }}, " +
                $"{(usesCommands ? "true" : "false")}, " +
                $"new global::TracyLive.Loader.ResourceAccessRegistration[] " +
                $"{{ {string.Join(", ", resourceAccesses)} }}, " +
                $"{runMethod})");
            return name;
        }

        /// <summary>Emit one startup's runner and registry entry.</summary>
        private static void EmitStartup(
            StringBuilder source,
            IMethodSymbol method,
            string receiver,
            int index,
            List<string> entries)
        {
            string runMethod = $"RunStartup_{index}";
            source.AppendLine(
                $"        private static void {runMethod}() => {receiver}.{method.Name}(default);");
            string name = $"{method.ContainingType.ToDisplayString(FullyQualified)}.{method.Name}";
            entries.Add(
                $"            new global::TracyLive.Loader.AotStartupRegistration(" +
                $"\"{name}\", {runMethod})");
        }
    }
}
