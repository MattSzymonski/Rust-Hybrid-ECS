# Rust Hybrid ECS

A minimal Entity Component System (ECS) with script components and sprite rendering.

## Features

### Core ECS Architecture
- **Entities**: Unique identifiers for game objects
- **Components**: Data containers (Position, Velocity, Sprite, Name, etc.)
- **World**: Central manager for entities and components
- **Type-safe storage**: Component storage using `TypeId` and `Any`

### Script Components
- Script components have `update()` functions that run automatically
- No traditional systems needed - logic lives in the components themselves
- `UpdateContext` allows scripts to schedule mutations that are applied after all scripts run
- Multiple script components can be attached to entities

### Sprite Rendering
- **Sprite component**: Defines color and size for visual representation
- **Renderer**: Uses `flo_draw` to display entities on screen
- Real-time rendering with update loop
- Square sprites with colored outlines and center points

## Running the Examples

### Console Example (Default)
```bash
cargo run
```
This runs a console-based example demonstrating:
- Entity creation and component management
- Query system for filtering entities
- Script component updates
- Position updates over multiple frames

### Sprite Rendering Example
```bash
cargo run -- render
```
This opens a window and displays moving sprites:
- 5 entities with different colors and behaviors
- 4 moving sprites with velocities
- 1 static sprite at the center
- Real-time updates at ~60 FPS
- Console logging every 60 frames

## Project Structure

```
src/
├── main.rs          - Entry point
├── ecs_core.rs      - Core ECS implementation
├── example.rs       - Example usage and demos
└── renderer.rs      - Sprite rendering with flo_draw
```

## Components

### Data Components
- **Position**: `(x: f32, y: f32)` - Entity position in world space
- **Velocity**: `(dx: f32, dy: f32)` - Movement direction and speed
- **Sprite**: `(color: (f32, f32, f32), size: f32)` - Visual representation
- **Name**: `String` - Entity identifier

### Script Components
- **MoverScript**: Moves entities based on their Velocity component
- **LoggerScript**: Logs messages to console

## Architecture Highlights

This is a **hybrid** between traditional ECS and Unity/Godot-style component scripts:
- Data components store state (Position, Velocity)
- Script components contain behavior (MoverScript, LoggerScript)
- No separate system functions needed
- `world.update_scripts()` runs all script components automatically

## Dependencies

- `flo_draw` - 2D graphics and windowing
- `flo_canvas` - Canvas drawing primitives
