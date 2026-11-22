package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual dimension-seeded BlendedNoise, preserving double output bits. */
public final class BlendedNoiseOracle {
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> noise=Class.forName("eoe"),random=Class.forName("dyz"),point=Class.forName("dyr$e");
        Constructor<?> ctor=noise.getConstructor(Class.forName("ayw"),double.class,double.class,double.class,double.class,double.class);
        Constructor<?> pos=point.getConstructor(int.class,int.class,int.class);
        Method compute=noise.getMethod("a",Class.forName("dyr$b"));
        long[] seeds={0,1,-1,42,Long.MIN_VALUE,Long.MAX_VALUE};
        double[][] configs={{0.25,0.25,80,160,4},{0.25,0.375,80,60,8},{0.25,0.125,80,160,8}};
        List<int[]> coords=new ArrayList<>(List.of(new int[]{0,0,0},new int[]{0,64,0},new int[]{-8,-4,8},new int[]{30000000,127,-30000000}));
        Random rng=new Random(1211042);
        for(int i=0;i<96;i++)coords.add(new int[]{rng.nextInt(65537)-32768,rng.nextInt(512)-128,rng.nextInt(65537)-32768});
        List<String> rows=new ArrayList<>();
        for(long seed:seeds)for(int c=0;c<configs.length;c++) {
            double[] p=configs[c];
            Object source=random.getConstructor(long.class).newInstance(seed);
            if(c==2) {
                Object root=Class.forName("dzz").getConstructor(long.class).newInstance(seed);
                Object factory=Class.forName("ayw").getMethod("e").invoke(root);
                Object id=Class.forName("akr").getMethod("a",String.class).invoke(null,"minecraft:terrain");
                source=Class.forName("dzl").getMethod("a",Class.forName("akr")).invoke(factory,id);
            }
            Object fn=ctor.newInstance(source,p[0],p[1],p[2],p[3],p[4]);
            for(int[] xyz:coords) {
                double v=(double)compute.invoke(fn,pos.newInstance(xyz[0],xyz[1],xyz[2]));
                rows.add(seed+"\t"+c+"\t"+xyz[0]+"\t"+xyz[1]+"\t"+xyz[2]+"\t"+Long.toUnsignedString(Double.doubleToRawLongBits(v),16));
            }
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println("native dimension blended noise cases: "+rows.size());
    }
}
