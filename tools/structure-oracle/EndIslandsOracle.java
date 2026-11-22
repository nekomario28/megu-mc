package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual EndIslandDensityFunction, retaining float/double output bits. */
public final class EndIslandsOracle {
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> density=Class.forName("dys$i"),context=Class.forName("dyr$e");
        Constructor<?> ctor=density.getConstructor(long.class),point=context.getConstructor(int.class,int.class,int.class);
        Method compute=density.getMethod("a",Class.forName("dyr$b"));
        Method height=density.getDeclaredMethod("a",Class.forName("eok"),int.class,int.class);height.setAccessible(true);
        Field noise=density.getDeclaredField("f");noise.setAccessible(true);
        long[] seeds={0L,1L,-1L,42L,Long.MIN_VALUE,Long.MAX_VALUE,0x123456789abcdef0L};
        List<int[]> coords=new ArrayList<>(List.of(new int[]{0,0},new int[]{1,-1},new int[]{-1,1},new int[]{7,-7},new int[]{8,-8},new int[]{63,64},new int[]{64,64},new int[]{65,65},new int[]{-63,-64},new int[]{-64,-64},new int[]{-65,-65},new int[]{512,512},new int[]{1024,-1024},new int[]{30000000,-30000000},new int[]{Integer.MIN_VALUE,Integer.MAX_VALUE},new int[]{Integer.MAX_VALUE,Integer.MIN_VALUE}));
        Random random=new Random(1211001);
        for(int i=0;i<64;i++)coords.add(new int[]{random.nextInt(32769)-16384,random.nextInt(32769)-16384});
        List<String> rows=new ArrayList<>();
        for(long seed:seeds) {
            Object fn=ctor.newInstance(seed),simplex=noise.get(fn);
            for(int[] pos:coords) {
                float h=(float)height.invoke(null,simplex,pos[0],pos[1]);
                double d=(double)compute.invoke(fn,point.newInstance(pos[0],64,pos[1]));
                rows.add(seed+"\t"+pos[0]+"\t"+pos[1]+"\t"+Integer.toUnsignedString(Float.floatToRawIntBits(h),16)+"\t"+Long.toUnsignedString(Double.doubleToRawLongBits(d),16));
            }
        }
        Files.write(Path.of(args[0]),rows);System.out.println("native End island density/height cases: "+rows.size());
    }
}
