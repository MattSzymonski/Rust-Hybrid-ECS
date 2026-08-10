using System.Diagnostics;

namespace TracyLive;

public static class BallPhysicsSystem
{
    private const float Gravity = 800.0f;
    private const float Restitution = 0.7f;
    private const float FloorY = 580.0f;
    private const float CeilingY = 20.0f;
    private const float LeftWall = 20.0f;
    private const float RightWall = 780.0f;
    private const float BounceVelocityY = -500.0f;
    private const float BounceVelocityX = 150.0f;

    private static long _lastFrame = Stopwatch.GetTimestamp();

    [EcsSystem]
    public static void Run(
        Query<EntityTerm, Write<PhysicsState>, Write<Position>, Write<Sprite>> query)
    {
        long now = Stopwatch.GetTimestamp();
        float deltaTime = Math.Clamp(
            (float)Stopwatch.GetElapsedTime(_lastFrame, now).TotalSeconds,
            0.0f,
            0.1f);
        _lastFrame = now;

        foreach (var components in query)
        {
            ref var physics = ref components.Write<PhysicsState>();
            ref var position = ref components.Write<Position>();
            ref var sprite = ref components.Write<Sprite>();

            // PhysicsState is a game-owned dynamic component. Native startup
            // only default-constructs it; all semantic initialization belongs
            // here and uses the stable entity ID to make each ball distinct.
            if (physics.Radius == 0.0f)
            {
                int index = (int)(components.Entity.Id % 100);
                float column = index % 10;
                float row = index / 10;
                physics.PositionX = 60.0f + column * 72.0f;
                physics.PositionY = 60.0f + row * 42.0f;
                physics.VelocityX = index % 2 == 0
                    ? BounceVelocityX + row * 8.0f
                    : -BounceVelocityX - row * 8.0f;
                physics.VelocityY = BounceVelocityY + column * 18.0f;
                physics.Radius = 10.0f + index % 4 * 2.0f;
                physics.Active = 1;
            }

            physics.DeltaTime = deltaTime;
            if (physics.Active != 0)
            {
                physics.VelocityY += Gravity * deltaTime;
                physics.PositionX += physics.VelocityX * deltaTime;
                physics.PositionY += physics.VelocityY * deltaTime;

                if (physics.PositionY + physics.Radius >= FloorY)
                {
                    physics.PositionY = FloorY - physics.Radius;
                    physics.VelocityY = -MathF.Abs(physics.VelocityY) * Restitution;
                    if (MathF.Abs(physics.VelocityY) < 10.0f)
                        physics.VelocityY = 0.0f;
                }
                if (physics.PositionY - physics.Radius <= CeilingY)
                {
                    physics.PositionY = CeilingY + physics.Radius;
                    physics.VelocityY = MathF.Abs(physics.VelocityY) * Restitution;
                }
                if (physics.PositionX - physics.Radius <= LeftWall)
                {
                    physics.PositionX = LeftWall + physics.Radius;
                    physics.VelocityX = MathF.Abs(physics.VelocityX) * Restitution;
                }
                if (physics.PositionX + physics.Radius >= RightWall)
                {
                    physics.PositionX = RightWall - physics.Radius;
                    physics.VelocityX = -MathF.Abs(physics.VelocityX) * Restitution;
                }
            }

            position.X = physics.PositionX - physics.Radius;
            position.Y = physics.PositionY - physics.Radius;
            sprite.Width = physics.Radius * 2.0f;
            sprite.Height = physics.Radius * 2.0f;
            sprite.Color = physics.Active != 0
                ? new Color { R = 1.0f, G = 0.3f, B = 0.3f, A = 1.0f }
                : new Color { R = 0.5f, G = 0.5f, B = 0.5f, A = 1.0f };
        }
    }
}

public static class BallTagSystem
{
    [EcsSystem]
    public static void Observe(Query<Read<BallTag>> query)
    {
        // Declaring and iterating this type exercises automatic registration
        // of a component owned entirely by game_cs.
        foreach (var row in query)
            _ = row.Read<BallTag>().Kind;
    }
}
