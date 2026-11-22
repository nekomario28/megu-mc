package dev.megu.tools;
import java.lang.reflect.*;
/** Native `StructureTemplate.transform(Vec3, Mirror, Rotation, BlockPos)` for entity offsets. */
public final class TemplateEntityOracle {
    static Class<?> C(String n) { try { return Class.forName(n); } catch (ClassNotFoundException e) { throw new AssertionError(e); } }
    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> vec = C("exc"), mirror = C("dkv"), rotation = C("dmm"), pos = C("jd"), template = C("ent");
        Method transform = template.getMethod("a", vec, mirror, rotation, pos);
        // Indices, not names: a template file stores the index, and the port's variant
        // names are swapped relative to native's (see structure_transform.rs).
        String[] mirrorNames = {"none", "front_back", "left_right"};
        Object[] mirrorValues = {mirror.getField("a").get(null), mirror.getField("b").get(null), mirror.getField("c").get(null)};
        String[] rotationNames = {"none", "clockwise_90", "clockwise_180", "counterclockwise_90"};
        Object[] rotationValues = {rotation.getField("a").get(null), rotation.getField("b").get(null),
                rotation.getField("c").get(null), rotation.getField("d").get(null)};
        double[][] offsets = {{0, 0, 0}, {1, 0, 0}, {0, 0, 1}, {1, 1, 1}, {0.5, 0.5, 0.5},
                {1.25, 2.0, 3.75}, {-1.5, 4.0, 0.25}, {7.0, -3.0, -2.5}, {0.25, 0.75, -4.5}};
        // Two pivots, because a candidate that ignores the pivot passes on one.
        Object[] pivots = {
            pos.getConstructor(int.class, int.class, int.class).newInstance(10, 64, -4),
            pos.getConstructor(int.class, int.class, int.class).newInstance(0, 0, 0)};
        for (int pi = 0; pi < pivots.length; pi++) {
        Object pivot = pivots[pi];
        for (int m = 0; m < mirrorValues.length; m++) {
            for (int r = 0; r < rotationValues.length; r++) {
                for (double[] o : offsets) {
                    Object v = vec.getConstructor(double.class, double.class, double.class)
                            .newInstance(o[0], o[1], o[2]);
                    Object out = transform.invoke(null, v, mirrorValues[m], rotationValues[r], pivot);
                    double x = (double) vec.getField("c").get(out);
                    double y = (double) vec.getField("d").get(out);
                    double z = (double) vec.getField("e").get(out);
                    System.out.println("ENT\t" + pi + "\t" + m + "\t" + r + "\t" + mirrorNames[m] + "\t" + rotationNames[r] + "\t"
                            + o[0] + "\t" + o[1] + "\t" + o[2] + "\t" + x + "\t" + y + "\t" + z);
                }
            }
        }
        }
    }
}
