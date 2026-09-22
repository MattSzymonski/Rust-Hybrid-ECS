// Components owned by this project.
//
// Managed counterpart of examples/italian_brainrot/src/lib.rs's
// `TagAlphaComponent`: a tag marking which entities the rotation system
// animates. The native definition is a genuinely zero-sized `#[repr(C)]`
// struct shared with hot-patched Rust modules through `trait_type_map`; a
// managed struct cannot be zero-sized (the CLR gives every struct a minimum
// size of one byte) and this project has no Rust-side sharing to match
// anyway, so the marker carries one unused byte instead.
//
// No [EcsSharedComponent]: that attribute is only for components the host
// binds by its own canonical schema (renderer/runtime mirrors). A plain
// project-owned component like this one is discovered automatically from its
// use in a query or Commands call.

using System.Runtime.InteropServices;

namespace TracyLive;

/// <summary>Marks the entities the rotation system animates.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct TagAlpha
{
    private byte _unused;
}

