package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** NormalNoise.create with WorldgenRandom(LegacyRandomSource), not the
 * createLegacyNetherBiome constructor. Samples from the pinned game classes.
 */
public class NormalNoiseOracle {
    public static void main(String[] args) throws Exception {
        Class<?> random=Class.forName("ayw"), legacy=Class.forName("dyz"), worldgen=Class.forName("dzx"), noise=Class.forName("eoh");
        Constructor<?> lcg=legacy.getConstructor(long.class), wrapper=worldgen.getConstructor(random);
        Method create=noise.getMethod("a",random,int.class,double[].class);
        Method get=noise.getMethod("a",double.class,double.class,double.class);
        int[] first={-4,-2,-3,-2,1,-2};
        double[][] amplitudes={{1},{1,.5},{1,0,.25},{0,1},{1},{0,0}};
        double[][] positions={{0,0,0},{1.25,-2.5,3.75},{-982,-35,520},{-969,-30,524},{1264,33,-16},{-1024,70,527},{233.5,-64.25,-412.125}};
        List<String> rows=new ArrayList<>();
        for(long seed:new long[]{0,1,11,60007}) {
            for(int c=0;c<first.length;c++) {
                Object n=create.invoke(null,wrapper.newInstance(lcg.newInstance(seed)),first[c],amplitudes[c]);
                for(double[] p:positions) {
                    double value=(double)get.invoke(n,p[0],p[1],p[2]);
                    rows.add(seed+"\t"+c+"\t"+p[0]+"\t"+p[1]+"\t"+p[2]+"\t"+Long.toUnsignedString(Double.doubleToRawLongBits(value),16));
                }
            }
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println("normal noise oracle: "+rows.size()+" actual class samples");
    }
}
