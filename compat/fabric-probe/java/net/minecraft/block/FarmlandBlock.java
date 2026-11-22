package net.minecraft.block;
import net.minecraft.entity.Entity;
import net.minecraft.world.World;
import net.minecraft.util.math.BlockPos;
import net.megu.probe.Host;
/** Only the already-eligible dirt-conversion branch; not Vanilla landing eligibility. */
public final class FarmlandBlock {
    public void onLandedUpon(World world, BlockState state, BlockPos pos, Entity entity, float distance) {
        if (!Float.isFinite(distance) || distance <= 0.5f) { throw new IllegalArgumentException("Contract scenario requires an eligible fall"); }
        setToDirt(entity, state, world, pos);
    }
    public static void setToDirt(Entity entity, BlockState state, World world, BlockPos pos) {
        Host.setBlock(pos.x(), pos.y(), pos.z(), 10);
    }
}
