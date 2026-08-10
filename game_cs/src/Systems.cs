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

    private static long _lastFrame = Stopwatch.GetTimestamp();

    [EcsSystem]
    public static void Run(Write3Query<PhysicsState, Position, Sprite> query)
    {
        long now = Stopwatch.GetTimestamp();
        float deltaTime = Math.Clamp(
            (float)Stopwatch.GetElapsedTime(_lastFrame, now).TotalSeconds,
            0.0f,
            0.1f);
        _lastFrame = now;

        foreach (var components in query)
        {
            ref var physics = ref components.First;
            ref var position = ref components.Second;
            ref var sprite = ref components.Third;

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
