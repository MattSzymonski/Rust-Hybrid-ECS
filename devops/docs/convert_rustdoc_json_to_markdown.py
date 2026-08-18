"""Convert every generated rustdoc JSON file into a separate Markdown page.

The input and output locations are intentionally hardcoded for the local Pill
Engine repositories. Run ``generate_rustdoc_json.py`` first, then execute:

    python devops/docs/convert_rustdoc_json_to_markdown.py

The converter validates and renders every input before deleting any existing
reference output. It then purges the hardcoded reference directory and writes
one ``.md`` file per rustdoc ``.json`` file.

Rustdoc JSON is an unstable nightly interface. Unknown future type variants are
rendered conservatively instead of aborting the entire conversion.
"""

import json
import re
import shutil
import sys
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Sequence, Set, Tuple


# =============================================================================
# Hardcoded repository locations
# =============================================================================

WORKSPACE_ROOT = Path(__file__).resolve().parents[2]
INPUT_DIRECTORY = WORKSPACE_ROOT / "target" / "doc"
OUTPUT_DIRECTORY = Path(
    r"Z:\OtherProjects\Other\Pill-Engine-Website\guide\pages\code-reference"
)

# Primary items appear as full entries in a crate page. Fields, variants, and
# associated items are rendered beneath their owning primary item instead.
PRIMARY_ITEM_KINDS = {
    "module",
    "struct",
    "enum",
    "union",
    "trait",
    "trait_alias",
    "function",
    "type_alias",
    "constant",
    "static",
    "macro",
    "proc_macro",
    "extern_type",
    "primitive",
}

ITEM_KIND_ORDER = [
    "module",
    "struct",
    "enum",
    "union",
    "trait",
    "trait_alias",
    "function",
    "type_alias",
    "constant",
    "static",
    "macro",
    "proc_macro",
    "extern_type",
    "primitive",
]

ITEM_KIND_TITLES = {
    "module": "Modules",
    "struct": "Structs",
    "enum": "Enums",
    "union": "Unions",
    "trait": "Traits",
    "trait_alias": "Trait aliases",
    "function": "Functions",
    "type_alias": "Type aliases",
    "constant": "Constants",
    "static": "Statics",
    "macro": "Macros",
    "proc_macro": "Procedural macros",
    "extern_type": "External types",
    "primitive": "Primitive types",
}


# =============================================================================
# General helpers
# =============================================================================

def item_kind(item: Dict[str, Any]) -> str:
    """Return the single rustdoc item variant stored inside ``inner``."""
    inner = item.get("inner", {})
    return next(iter(inner), "unknown")


def item_identifier(value: Any) -> str:
    """Normalize numeric or string rustdoc IDs for dictionary lookup."""
    return str(value)


def markdown_anchor(identifier: Any) -> str:
    """Create a deterministic anchor independent of VitePress slug rules."""
    return "rustdoc-item-{}".format(item_identifier(identifier))


def code_span(value: str) -> str:
    """Wrap text in a Markdown code span, safely handling embedded backticks."""
    if "`" not in value:
        return "`{}`".format(value)
    return "``{}``".format(value)


def first_documentation_line(documentation: Optional[str]) -> str:
    """Return a compact summary suitable for a contents table."""
    if not documentation:
        return ""
    for line in documentation.splitlines():
        candidate = line.strip().lstrip("#").strip()
        if candidate:
            return candidate
    return ""


def demote_markdown_headings(documentation: str, offset: int) -> str:
    """Demote rustdoc headings so embedded docs fit the generated page outline.

    Headings inside fenced code blocks are deliberately left untouched.
    """
    output: List[str] = []
    inside_fence = False
    for line in documentation.splitlines():
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            inside_fence = not inside_fence
            output.append(line)
            continue

        match = None if inside_fence else re.match(r"^(#{1,6})(\s+.*)$", line)
        if match:
            level = min(6, len(match.group(1)) + offset)
            output.append("{}{}".format("#" * level, match.group(2)))
        else:
            output.append(line)
    return "\n".join(output)


# =============================================================================
# Rust type and signature rendering
# =============================================================================

def render_generic_argument(argument: Dict[str, Any]) -> str:
    """Render one generic argument from rustdoc's tagged JSON representation."""
    if "lifetime" in argument:
        return str(argument["lifetime"])
    if "type" in argument:
        return render_type(argument["type"])
    if "const" in argument:
        constant = argument["const"]
        if isinstance(constant, dict):
            return str(constant.get("expr") or constant.get("value") or "_")
        return str(constant)
    if "infer" in argument:
        return "_"
    return "_"


def render_generic_arguments(arguments: Optional[Dict[str, Any]]) -> str:
    """Render angle-bracketed or parenthesized path arguments."""
    if not arguments:
        return ""

    angle_bracketed = arguments.get("angle_bracketed")
    if angle_bracketed is not None:
        values = [
            render_generic_argument(argument)
            for argument in angle_bracketed.get("args", [])
        ]
        for constraint in angle_bracketed.get("constraints", []):
            name = constraint.get("name", "_")
            binding = constraint.get("binding") or constraint.get("constraint") or {}
            if "equality" in binding:
                values.append(
                    "{} = {}".format(name, render_type(binding["equality"]))
                )
            elif "bounds" in binding:
                values.append(
                    "{}: {}".format(name, render_bounds(binding["bounds"]))
                )
        return "<{}>".format(", ".join(values)) if values else ""

    parenthesized = arguments.get("parenthesized")
    if parenthesized is not None:
        inputs = ", ".join(
            render_type(input_type)
            for input_type in parenthesized.get("inputs", [])
        )
        output = parenthesized.get("output")
        suffix = " -> {}".format(render_type(output)) if output else ""
        return "({}){}".format(inputs, suffix)

    return ""


def render_bound(bound: Dict[str, Any]) -> str:
    """Render a trait, lifetime, or precise-capture generic bound."""
    if "outlives" in bound:
        return str(bound["outlives"])

    trait_bound = bound.get("trait_bound")
    if trait_bound is not None:
        trait = trait_bound.get("trait", {})
        modifier = trait_bound.get("modifier", "none")
        prefix = "?" if modifier == "maybe" else ""
        return "{}{}{}".format(
            prefix,
            trait.get("path", "_"),
            render_generic_arguments(trait.get("args")),
        )

    if "use" in bound:
        captures = bound["use"]
        if isinstance(captures, list):
            return "use<{}>".format(", ".join(map(str, captures)))
    return "_"


def render_bounds(bounds: Sequence[Dict[str, Any]]) -> str:
    """Render a plus-separated list of generic bounds."""
    return " + ".join(render_bound(bound) for bound in bounds)


def render_type(type_data: Any) -> str:
    """Render rustdoc's recursive ``Type`` enum as Rust-like source text."""
    if type_data is None:
        return "()"
    if isinstance(type_data, str):
        return type_data
    if not isinstance(type_data, dict):
        return "_"

    if "primitive" in type_data:
        return str(type_data["primitive"])
    if "generic" in type_data:
        return str(type_data["generic"])
    if "infer" in type_data:
        return "_"
    if "never" in type_data:
        return "!"

    resolved_path = type_data.get("resolved_path")
    if resolved_path is not None:
        return "{}{}".format(
            resolved_path.get("path", "_"),
            render_generic_arguments(resolved_path.get("args")),
        )

    qualified_path = type_data.get("qualified_path")
    if qualified_path is not None:
        self_type = render_type(qualified_path.get("self_type"))
        trait = qualified_path.get("trait")
        if trait:
            owner = "{} as {}{}".format(
                self_type,
                trait.get("path", "_"),
                render_generic_arguments(trait.get("args")),
            )
        else:
            owner = self_type
        return "<{}>::{}{}".format(
            owner,
            qualified_path.get("name", "_"),
            render_generic_arguments(qualified_path.get("args")),
        )

    borrowed_reference = type_data.get("borrowed_ref")
    if borrowed_reference is not None:
        lifetime = borrowed_reference.get("lifetime")
        lifetime_text = "{} ".format(lifetime) if lifetime else ""
        mutable_text = "mut " if borrowed_reference.get("is_mutable") else ""
        return "&{}{}{}".format(
            lifetime_text,
            mutable_text,
            render_type(borrowed_reference.get("type")),
        )

    raw_pointer = type_data.get("raw_pointer")
    if raw_pointer is not None:
        qualifier = "mut" if raw_pointer.get("is_mutable") else "const"
        return "*{} {}".format(qualifier, render_type(raw_pointer.get("type")))

    if "tuple" in type_data:
        members = [render_type(member) for member in type_data["tuple"]]
        if len(members) == 1:
            return "({},)".format(members[0])
        return "({})".format(", ".join(members))
    if "slice" in type_data:
        return "[{}]".format(render_type(type_data["slice"]))
    if "array" in type_data:
        array = type_data["array"]
        return "[{}; {}]".format(
            render_type(array.get("type")), array.get("len", "_")
        )

    implementation_trait = type_data.get("impl_trait")
    if implementation_trait is not None:
        return "impl {}".format(render_bounds(implementation_trait))

    dynamic_trait = type_data.get("dyn_trait")
    if dynamic_trait is not None:
        traits = []
        for poly_trait in dynamic_trait.get("traits", []):
            trait = poly_trait.get("trait", {})
            traits.append(
                "{}{}".format(
                    trait.get("path", "_"),
                    render_generic_arguments(trait.get("args")),
                )
            )
        lifetime = dynamic_trait.get("lifetime")
        if lifetime:
            traits.append(str(lifetime))
        return "dyn {}".format(" + ".join(traits) or "_")

    function_pointer = type_data.get("function_pointer")
    if function_pointer is not None:
        header = render_function_header(function_pointer.get("header", {}))
        signature = render_function_signature(
            "",
            function_pointer.get("sig", {}),
            visibility="",
            header=header,
            generic_text="",
        )
        return signature.rstrip(";")

    # The JSON schema is explicitly unstable. A readable underscore is safer
    # than embedding implementation-shaped JSON into a Rust signature.
    return "_"


def render_generic_parameters(generics: Optional[Dict[str, Any]]) -> str:
    """Render generic parameter declarations, excluding where predicates."""
    if not generics:
        return ""
    parameters: List[str] = []
    for parameter in generics.get("params", []):
        name = parameter.get("name", "_")
        kind = parameter.get("kind", {})
        type_parameter = kind.get("type")
        if type_parameter is not None:
            bounds = render_bounds(type_parameter.get("bounds", []))
            default = type_parameter.get("default")
            text = "{}: {}".format(name, bounds) if bounds else name
            if default is not None:
                text += " = {}".format(render_type(default))
            parameters.append(text)
        elif "lifetime" in kind:
            outlives = kind["lifetime"].get("outlives", [])
            suffix = ": {}".format(" + ".join(outlives)) if outlives else ""
            parameters.append("{}{}".format(name, suffix))
        elif "const" in kind:
            constant = kind["const"]
            text = "const {}: {}".format(name, render_type(constant.get("type")))
            if constant.get("default") is not None:
                text += " = {}".format(constant["default"])
            parameters.append(text)
        else:
            parameters.append(name)
    return "<{}>".format(", ".join(parameters)) if parameters else ""


def render_where_predicates(generics: Optional[Dict[str, Any]]) -> str:
    """Render the most common rustdoc where-predicate forms."""
    if not generics:
        return ""
    predicates: List[str] = []
    for predicate in generics.get("where_predicates", []):
        bound_predicate = predicate.get("bound_predicate")
        if bound_predicate is not None:
            bounds = render_bounds(bound_predicate.get("bounds", []))
            predicates.append(
                "{}: {}".format(render_type(bound_predicate.get("type")), bounds)
            )
            continue
        lifetime_predicate = predicate.get("lifetime_predicate")
        if lifetime_predicate is not None:
            predicates.append(
                "{}: {}".format(
                    lifetime_predicate.get("lifetime", "_"),
                    " + ".join(lifetime_predicate.get("outlives", [])),
                )
            )
            continue
        equality_predicate = predicate.get("eq_predicate")
        if equality_predicate is not None:
            predicates.append(
                "{} = {}".format(
                    render_type(equality_predicate.get("lhs")),
                    render_type(equality_predicate.get("rhs")),
                )
            )
    return " where {}".format(", ".join(predicates)) if predicates else ""


def render_visibility(visibility: Any) -> str:
    """Translate rustdoc visibility metadata to a Rust source prefix."""
    if visibility == "public":
        return "pub "
    if visibility == "crate":
        return "pub(crate) "
    if isinstance(visibility, dict) and "restricted" in visibility:
        path = visibility["restricted"].get("path", "crate")
        normalized = str(path).lstrip(":") or "crate"
        return "pub(in {}) ".format(normalized)
    return ""


def render_abi(abi: Any) -> str:
    """Render Rust or named external ABIs."""
    if abi in (None, "Rust"):
        return ""
    if isinstance(abi, str):
        return 'extern "{}" '.format(abi)
    if isinstance(abi, dict) and abi:
        name = next(iter(abi))
        unwind = abi[name].get("unwind") if isinstance(abi[name], dict) else False
        suffix = "-unwind" if unwind else ""
        return 'extern "{}{}" '.format(name, suffix)
    return ""


def render_function_header(header: Dict[str, Any]) -> str:
    """Render qualifiers appearing before the ``fn`` keyword."""
    qualifiers = []
    if header.get("is_const"):
        qualifiers.append("const")
    if header.get("is_async"):
        qualifiers.append("async")
    if header.get("is_unsafe"):
        qualifiers.append("unsafe")
    abi = render_abi(header.get("abi"))
    if abi:
        qualifiers.append(abi.strip())
    return "{} ".format(" ".join(qualifiers)) if qualifiers else ""


def render_function_signature(
    name: str,
    signature: Dict[str, Any],
    *,
    visibility: str,
    header: str,
    generic_text: str,
    where_text: str = "",
) -> str:
    """Render a function or function-pointer signature."""
    inputs = []
    for parameter_name, parameter_type in signature.get("inputs", []):
        if parameter_name:
            inputs.append("{}: {}".format(parameter_name, render_type(parameter_type)))
        else:
            inputs.append(render_type(parameter_type))
    if signature.get("is_c_variadic"):
        inputs.append("...")
    output = signature.get("output")
    output_text = " -> {}".format(render_type(output)) if output else ""
    return "{}{}fn {}{}({}){}{};".format(
        visibility,
        header,
        name,
        generic_text,
        ", ".join(inputs),
        output_text,
        where_text,
    )


# =============================================================================
# Markdown renderer for one rustdoc crate
# =============================================================================

class CrateMarkdownRenderer:
    """Turn one rustdoc crate graph into a self-contained Markdown document."""

    def __init__(self, document: Dict[str, Any], source_file: Path) -> None:
        self.document = document
        self.source_file = source_file
        self.index: Dict[str, Dict[str, Any]] = document["index"]
        self.root_identifier = item_identifier(document["root"])
        self.paths: Dict[str, str] = {}
        self.primary_identifiers: List[str] = []
        self.rendered_identifiers: Set[str] = set()
        self._discover_items()

    def get_item(self, identifier: Any) -> Optional[Dict[str, Any]]:
        """Find an item in rustdoc's string-keyed item index."""
        return self.index.get(item_identifier(identifier))

    def _discover_items(self) -> None:
        """Walk the module tree and establish canonical local item paths."""
        root = self.get_item(self.root_identifier)
        if root is None:
            raise ValueError("rustdoc root item is missing from the item index")
        crate_name = root.get("name") or self.source_file.stem
        self.paths[self.root_identifier] = crate_name
        visited_modules: Set[str] = set()

        def walk_module(identifier: str, module_path: str) -> None:
            if identifier in visited_modules:
                return
            visited_modules.add(identifier)
            module_item = self.get_item(identifier)
            if module_item is None:
                return
            module_data = module_item.get("inner", {}).get("module", {})
            for child_value in module_data.get("items", []):
                child_identifier = item_identifier(child_value)
                child = self.get_item(child_identifier)
                if child is None or child.get("crate_id") != 0:
                    continue
                kind = item_kind(child)
                name = child.get("name")
                if not name or kind in {"use", "impl"}:
                    continue
                child_path = "{}::{}".format(module_path, name)
                self.paths.setdefault(child_identifier, child_path)
                if kind in PRIMARY_ITEM_KINDS:
                    self.primary_identifiers.append(child_identifier)
                if kind == "module":
                    walk_module(child_identifier, child_path)

        walk_module(self.root_identifier, crate_name)

        # Public path summaries can supply canonical names for items referenced
        # by documentation links. Do not add those items to the primary list:
        # rustdoc also stores associated methods in ``paths``, and methods must
        # remain nested below their owning type instead of appearing twice as
        # module-level functions.
        for identifier, summary in self.document.get("paths", {}).items():
            normalized_identifier = item_identifier(identifier)
            if summary.get("crate_id") != 0:
                continue
            path = summary.get("path", [])
            if path:
                self.paths.setdefault(normalized_identifier, "::".join(path))

        self.primary_identifiers = list(dict.fromkeys(self.primary_identifiers))
        self.rendered_identifiers.update(self.primary_identifiers)
        self.rendered_identifiers.add(self.root_identifier)
        for identifier in self.primary_identifiers:
            self.rendered_identifiers.update(self._related_item_identifiers(identifier))

    def _related_item_identifiers(self, identifier: str) -> Set[str]:
        """Collect fields, variants, trait members, and inherent impl members."""
        item = self.get_item(identifier)
        if item is None:
            return set()
        kind = item_kind(item)
        data = item.get("inner", {}).get(kind, {})
        related: Set[str] = set()

        if kind == "struct":
            structure_kind = data.get("kind")
            if isinstance(structure_kind, dict):
                if "plain" in structure_kind:
                    related.update(
                        item_identifier(value)
                        for value in structure_kind["plain"].get("fields", [])
                    )
                elif "tuple" in structure_kind:
                    related.update(
                        item_identifier(value)
                        for value in structure_kind["tuple"]
                        if value is not None
                    )
        elif kind == "union":
            related.update(item_identifier(value) for value in data.get("fields", []))
        elif kind == "enum":
            related.update(item_identifier(value) for value in data.get("variants", []))
        elif kind == "trait":
            related.update(item_identifier(value) for value in data.get("items", []))

        if kind in {"struct", "enum", "union"}:
            for implementation_value in data.get("impls", []):
                implementation = self.get_item(implementation_value)
                if implementation is None:
                    continue
                implementation_data = implementation.get("inner", {}).get("impl", {})
                if implementation_data.get("trait") is None:
                    related.update(
                        item_identifier(value)
                        for value in implementation_data.get("items", [])
                    )
        return related

    def _resolve_documentation_links(
        self, documentation: str, item: Dict[str, Any]
    ) -> str:
        """Resolve rustdoc shortcut links when their targets are rendered here."""
        for label, target in item.get("links", {}).items():
            target_identifier = item_identifier(target)
            if target_identifier not in self.rendered_identifiers:
                continue
            original = "[{}]".format(label)
            replacement = "[{}](#{})".format(
                label, markdown_anchor(target_identifier)
            )
            documentation = documentation.replace(original, replacement)
        return documentation

    def _render_documentation(self, item: Dict[str, Any], heading_offset: int) -> str:
        documentation = item.get("docs")
        if not documentation:
            return "_No documentation provided._"
        resolved = self._resolve_documentation_links(documentation, item)
        return demote_markdown_headings(resolved, heading_offset)

    def _render_metadata(self, item: Dict[str, Any]) -> str:
        """Render compact visibility, source, and deprecation information."""
        values = []
        visibility = item.get("visibility")
        if visibility != "public":
            values.append("Visibility: {}".format(code_span(str(visibility))))
        span = item.get("span")
        if span:
            begin = span.get("begin", [None])
            line = begin[0] if begin else None
            location = span.get("filename", "")
            if line is not None:
                location = "{}:{}".format(location, line)
            values.append("Source: {}".format(code_span(location)))
        if item.get("deprecation") is not None:
            values.append("**Deprecated**")
        return " · ".join(values)

    def _render_function(self, item: Dict[str, Any]) -> str:
        data = item["inner"]["function"]
        generics = data.get("generics", {})
        return render_function_signature(
            item.get("name") or "_",
            data.get("sig", {}),
            visibility=render_visibility(item.get("visibility")),
            header=render_function_header(data.get("header", {})),
            generic_text=render_generic_parameters(generics),
            where_text=render_where_predicates(generics),
        )

    def _render_signature(self, item: Dict[str, Any]) -> str:
        """Render a concise Rust-like declaration for a primary or child item."""
        kind = item_kind(item)
        data = item.get("inner", {}).get(kind, {})
        name = item.get("name") or "_"
        visibility = render_visibility(item.get("visibility"))

        if kind == "function":
            return self._render_function(item)
        if kind == "module":
            return "{}mod {};".format(visibility, name)
        if kind == "struct":
            generics = data.get("generics", {})
            return "{}struct {}{}{};".format(
                visibility,
                name,
                render_generic_parameters(generics),
                render_where_predicates(generics),
            )
        if kind == "enum":
            generics = data.get("generics", {})
            return "{}enum {}{}{} {{ ... }}".format(
                visibility,
                name,
                render_generic_parameters(generics),
                render_where_predicates(generics),
            )
        if kind == "union":
            generics = data.get("generics", {})
            return "{}union {}{}{} {{ ... }}".format(
                visibility,
                name,
                render_generic_parameters(generics),
                render_where_predicates(generics),
            )
        if kind == "trait":
            generics = data.get("generics", {})
            unsafe = "unsafe " if data.get("is_unsafe") else ""
            bounds = render_bounds(data.get("bounds", []))
            bounds_text = ": {}".format(bounds) if bounds else ""
            return "{}{}trait {}{}{}{} {{ ... }}".format(
                visibility,
                unsafe,
                name,
                render_generic_parameters(generics),
                bounds_text,
                render_where_predicates(generics),
            )
        if kind == "trait_alias":
            generics = data.get("generics", {})
            return "{}trait {}{} = {};".format(
                visibility,
                name,
                render_generic_parameters(generics),
                render_bounds(data.get("params", data.get("bounds", []))),
            )
        if kind == "type_alias":
            generics = data.get("generics", {})
            return "{}type {}{} = {}{};".format(
                visibility,
                name,
                render_generic_parameters(generics),
                render_type(data.get("type")),
                render_where_predicates(generics),
            )
        if kind == "constant":
            constant = data.get("const", {})
            value = constant.get("expr") or constant.get("value") or "_"
            return "{}const {}: {} = {};".format(
                visibility, name, render_type(data.get("type")), value
            )
        if kind == "static":
            mutable = "mut " if data.get("is_mutable") else ""
            expression = data.get("expr") or "_"
            return "{}static {}{}: {} = {};".format(
                visibility, mutable, name, render_type(data.get("type")), expression
            )
        if kind == "assoc_const":
            value = data.get("value")
            suffix = " = {}".format(value) if value is not None else ""
            return "const {}: {}{};".format(name, render_type(data.get("type")), suffix)
        if kind == "assoc_type":
            generics = data.get("generics", {})
            bounds = render_bounds(data.get("bounds", []))
            bounds_text = ": {}".format(bounds) if bounds else ""
            assigned_type = data.get("type")
            assignment = (
                " = {}".format(render_type(assigned_type))
                if assigned_type is not None
                else ""
            )
            return "type {}{}{}{};".format(
                name,
                render_generic_parameters(generics),
                bounds_text,
                assignment,
            )
        if kind == "struct_field":
            return "{}: {}".format(name, render_type(data))
        if kind == "variant":
            return name
        if kind == "macro":
            return str(data).strip() or "macro_rules! {} {{ ... }}".format(name)
        if kind == "proc_macro":
            return "proc_macro {}".format(name)
        if kind == "extern_type":
            return "{}extern type {};".format(visibility, name)
        if kind == "primitive":
            return name
        return name

    def _render_code_block(self, signature: str) -> str:
        return "```rust\n{}\n```".format(signature)

    def _child_identifiers(self, item: Dict[str, Any]) -> List[Tuple[str, List[str]]]:
        """Return labelled child groups for fields, variants, and trait members."""
        kind = item_kind(item)
        data = item.get("inner", {}).get(kind, {})
        groups: List[Tuple[str, List[str]]] = []

        if kind == "struct":
            structure_kind = data.get("kind")
            fields: List[str] = []
            if isinstance(structure_kind, dict):
                if "plain" in structure_kind:
                    fields = [
                        item_identifier(value)
                        for value in structure_kind["plain"].get("fields", [])
                    ]
                elif "tuple" in structure_kind:
                    fields = [
                        item_identifier(value)
                        for value in structure_kind["tuple"]
                        if value is not None
                    ]
            if fields:
                groups.append(("Fields", fields))
        elif kind == "union":
            groups.append(
                ("Fields", [item_identifier(value) for value in data.get("fields", [])])
            )
        elif kind == "enum":
            groups.append(
                (
                    "Variants",
                    [item_identifier(value) for value in data.get("variants", [])],
                )
            )
        elif kind == "trait":
            groups.append(
                (
                    "Required and provided items",
                    [item_identifier(value) for value in data.get("items", [])],
                )
            )

        if kind in {"struct", "enum", "union"}:
            inherent_items: List[str] = []
            for implementation_identifier in data.get("impls", []):
                implementation = self.get_item(implementation_identifier)
                if implementation is None:
                    continue
                implementation_data = implementation.get("inner", {}).get("impl", {})
                if implementation_data.get("trait") is None:
                    inherent_items.extend(
                        item_identifier(value)
                        for value in implementation_data.get("items", [])
                    )
            if inherent_items:
                groups.append(("Associated functions and methods", inherent_items))

        return [(label, identifiers) for label, identifiers in groups if identifiers]

    def _render_child_groups(self, item: Dict[str, Any]) -> str:
        sections: List[str] = []
        seen: Set[str] = set()
        for label, identifiers in self._child_identifiers(item):
            child_sections: List[str] = []
            for identifier in identifiers:
                if identifier in seen:
                    continue
                seen.add(identifier)
                child = self.get_item(identifier)
                if child is None:
                    continue
                name = child.get("name") or "Unnamed item"
                child_sections.extend(
                    [
                        '<a id="{}"></a>'.format(markdown_anchor(identifier)),
                        "##### {}".format(code_span(name)),
                        self._render_code_block(self._render_signature(child)),
                        self._render_documentation(child, heading_offset=5),
                    ]
                )
            if child_sections:
                sections.append("#### {}".format(label))
                sections.extend(child_sections)
        return "\n\n".join(sections)

    def _render_trait_implementations(self, item: Dict[str, Any]) -> str:
        kind = item_kind(item)
        if kind not in {"struct", "enum", "union"}:
            return ""
        data = item["inner"][kind]
        implementations = []
        seen: Set[str] = set()
        for implementation_identifier in data.get("impls", []):
            implementation = self.get_item(implementation_identifier)
            if implementation is None:
                continue
            implementation_data = implementation.get("inner", {}).get("impl", {})
            trait = implementation_data.get("trait")
            if trait is None or implementation_data.get("is_synthetic"):
                continue
            trait_text = "{}{}".format(
                trait.get("path", "_"),
                render_generic_arguments(trait.get("args")),
            )
            if trait_text in seen:
                continue
            seen.add(trait_text)
            prefix = "!" if implementation_data.get("is_negative") else ""
            implementations.append("- {}".format(code_span(prefix + trait_text)))
        if not implementations:
            return ""
        return "#### Trait implementations\n\n{}".format("\n".join(implementations))

    def _render_primary_item(self, identifier: str) -> str:
        item = self.get_item(identifier)
        if item is None:
            return ""
        path = self.paths.get(identifier, item.get("name") or identifier)
        sections = [
            '<a id="{}"></a>'.format(markdown_anchor(identifier)),
            "### {}".format(code_span(path)),
            self._render_code_block(self._render_signature(item)),
        ]
        metadata = self._render_metadata(item)
        if metadata:
            sections.append(metadata)
        sections.append(self._render_documentation(item, heading_offset=3))

        child_groups = self._render_child_groups(item)
        if child_groups:
            sections.append(child_groups)
        implementations = self._render_trait_implementations(item)
        if implementations:
            sections.append(implementations)
        return "\n\n".join(sections)

    def render(self) -> str:
        """Render the complete crate page."""
        root = self.get_item(self.root_identifier)
        if root is None:
            raise ValueError("rustdoc root item is missing")
        crate_name = root.get("name") or self.source_file.stem
        version = self.document.get("crate_version") or "unspecified"
        format_version = self.document.get("format_version", "unknown")

        lines = [
            "<!-- This file is generated. Do not edit it directly. -->",
            "",
            "# {} API reference".format(crate_name),
            "",
            "> Crate version: {} · Rustdoc JSON format: {} · Private items: {}".format(
                code_span(str(version)),
                code_span(str(format_version)),
                "included" if self.document.get("includes_private") else "excluded",
            ),
            "",
            self._render_documentation(root, heading_offset=1),
        ]

        grouped: Dict[str, List[str]] = {kind: [] for kind in ITEM_KIND_ORDER}
        for identifier in self.primary_identifiers:
            item = self.get_item(identifier)
            if item is not None:
                grouped.setdefault(item_kind(item), []).append(identifier)

        # Emit a compact linked contents list before the detailed declarations.
        contents = []
        for kind in ITEM_KIND_ORDER:
            identifiers = grouped.get(kind, [])
            if not identifiers:
                continue
            contents.append("- **{}**".format(ITEM_KIND_TITLES[kind]))
            for identifier in sorted(
                identifiers, key=lambda value: self.paths.get(value, value).lower()
            ):
                item = self.get_item(identifier) or {}
                path = self.paths.get(identifier, item.get("name") or identifier)
                summary = first_documentation_line(item.get("docs"))
                suffix = " — {}".format(summary) if summary else ""
                contents.append(
                    "  - [{}](#{}){}".format(
                        code_span(path), markdown_anchor(identifier), suffix
                    )
                )
        if contents:
            lines.extend(["", "## Contents", "", "\n".join(contents)])

        for kind in ITEM_KIND_ORDER:
            identifiers = grouped.get(kind, [])
            if not identifiers:
                continue
            lines.extend(["", "## {}".format(ITEM_KIND_TITLES[kind]), ""])
            rendered_items = [
                self._render_primary_item(identifier)
                for identifier in sorted(
                    identifiers, key=lambda value: self.paths.get(value, value).lower()
                )
            ]
            lines.append("\n\n---\n\n".join(filter(None, rendered_items)))

        return "\n".join(lines).rstrip() + "\n"


# =============================================================================
# Input validation, destructive output preparation, and sequential writes
# =============================================================================

def discover_input_files() -> List[Path]:
    """Return all generated crate JSON files without recursing into subfolders."""
    if not INPUT_DIRECTORY.is_dir():
        raise FileNotFoundError("rustdoc directory does not exist: {}".format(INPUT_DIRECTORY))
    files = sorted(INPUT_DIRECTORY.glob("*.json"), key=lambda path: path.name.lower())
    if not files:
        raise FileNotFoundError(
            "no rustdoc JSON files found under {}".format(INPUT_DIRECTORY)
        )
    return files


def load_and_render(input_file: Path) -> Tuple[str, str]:
    """Parse and render one input, returning its output filename and Markdown."""
    with input_file.open("r", encoding="utf-8") as stream:
        document = json.load(stream)
    required_keys = {"root", "index", "format_version"}
    missing_keys = required_keys.difference(document)
    if missing_keys:
        raise ValueError(
            "{} is missing rustdoc keys: {}".format(
                input_file, ", ".join(sorted(missing_keys))
            )
        )
    renderer = CrateMarkdownRenderer(document, input_file)
    return "{}.md".format(input_file.stem), renderer.render()


def purge_output_directory() -> None:
    """Delete every existing entry from the exact hardcoded output directory."""
    expected = Path(
        r"Z:\OtherProjects\Other\Pill-Engine-Website\guide\pages\reference"
    ).resolve()
    resolved = OUTPUT_DIRECTORY.resolve()

    # Guard the recursive operation even though the value is hardcoded. Any
    # future edit that points elsewhere must fail closed instead of deleting a
    # broader directory such as ``pages`` or the guide repository root.
    if resolved != expected or resolved.name != "reference":
        raise RuntimeError("refusing to purge unexpected path: {}".format(resolved))

    resolved.mkdir(parents=True, exist_ok=True)
    for entry in resolved.iterdir():
        if entry.is_symlink() or entry.is_file():
            entry.unlink()
        elif entry.is_dir():
            shutil.rmtree(str(entry))
        else:
            raise RuntimeError("unsupported output entry: {}".format(entry))


def write_output_file(filename: str, markdown: str) -> Path:
    """Write one UTF-8 Markdown file atomically into the reference directory."""
    destination = OUTPUT_DIRECTORY / filename
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    with temporary.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(markdown)
    temporary.replace(destination)
    return destination


def main() -> int:
    """Validate all inputs, purge the output directory, and write pages in order."""
    try:
        input_files = discover_input_files()

        # Render everything before the destructive purge. A malformed or
        # unsupported JSON input therefore leaves the current reference pages
        # untouched rather than replacing them with an empty/partial directory.
        rendered_outputs = []
        for input_file in input_files:
            print("Validating {}...".format(input_file.name), flush=True)
            rendered_outputs.append(load_and_render(input_file))

        print("Purging {}...".format(OUTPUT_DIRECTORY), flush=True)
        purge_output_directory()

        for index, (filename, markdown) in enumerate(rendered_outputs, start=1):
            destination = write_output_file(filename, markdown)
            print(
                "[{}/{}] Wrote {}".format(index, len(rendered_outputs), destination),
                flush=True,
            )
    except (
        OSError,
        RuntimeError,
        ValueError,
        KeyError,
        TypeError,
        json.JSONDecodeError,
    ) as error:
        print("Conversion failed: {}".format(error), file=sys.stderr)
        return 1

    print("Converted {} rustdoc JSON file(s).".format(len(rendered_outputs)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
