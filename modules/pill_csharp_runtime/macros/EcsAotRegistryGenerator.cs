// Roslyn source generator for the NativeAOT shipping posture.
//
// The reflection-based system discovery in `ProjectHost` cannot run under
// NativeAOT: `Activator.CreateInstance`, `Expression.Compile` and
// `MethodInfo.Invoke` all require dynamic code or reflection invokers that
// AOT does not provide. This generator instead emits, at compile time, a
// direct registration table for every `[EcsSystem]` / `[EcsStartup]` method:
// a static runner per system that constructs the query with `new` and calls
// the method directly (no reflection), plus the query descriptor needed for
// scheduler access derivation and the component manifest.
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
        // FullyQualifiedFormat renders with the `global::` prefix, so the
        // comparison strings must carry it too.
        private const string CommandsTypeName = "global::TracyLive.Commands";
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
                            "System {0} has an unsupported query parameter for the AOT registry",
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

        /// <summary>Emit one system's query holder, runner, and registry entry.</summary>
        private static string? EmitSystem(
            StringBuilder source,
            IMethodSymbol method,
            string receiver,
            int index,
            List<string> entries)
        {
            // Classify parameters: at most two, each Commands or a query.
            IParameterSymbol? queryParameter = null;
            bool usesCommands = false;
            foreach (IParameterSymbol parameter in method.Parameters)
            {
                if (IsCommands(parameter.Type))
                {
                    usesCommands = true;
                    continue;
                }
                if (queryParameter is not null)
                    return null; // more than one query parameter: unsupported
                if (!IsQueryType(parameter.Type))
                    return null; // non-query, non-Commands parameter: unsupported
                queryParameter = parameter;
            }
            if (queryParameter is null && !usesCommands)
                return null; // no usable parameters at all

            string queryType = queryParameter is null
                ? "global::System.Object"
                : queryParameter.Type.ToDisplayString(FullyQualified);
            string queryField = $"s_query_{index}";
            string runMethod = $"RunSystem_{index}";

            if (queryParameter is not null)
            {
                source.AppendLine($"        private static readonly {queryType} {queryField} = new {queryType}();");
            }

            // Build the invocation argument list in declared parameter order.
            List<string> arguments = new();
            foreach (IParameterSymbol parameter in method.Parameters)
            {
                if (IsCommands(parameter.Type))
                    arguments.Add("default");
                else
                    arguments.Add(queryField);
            }
            string args = string.Join(", ", arguments);
            source.AppendLine(
                $"        private static void {runMethod}() => {receiver}.{method.Name}({args});");

            string descriptor = queryParameter is null
                ? "null"
                : $"{queryField}.Descriptor";
            string name = $"{method.ContainingType.ToDisplayString(FullyQualified)}.{method.Name}";
            entries.Add(
                $"            new global::TracyLive.Loader.AotSystemRegistration(" +
                $"\"{name}\", {descriptor}, {(usesCommands ? "true" : "false")}, {runMethod})");
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
