package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Executes native block/entity coordinate transforms, bounds and every state transform. */
public final class StructureTransformOracle {
    static String ints(int... v) { return Arrays.stream(v).mapToObj(Integer::toString).collect(java.util.stream.Collectors.joining(",")); }
    static String bits(double... v) { return Arrays.stream(v).mapToObj(d->Long.toUnsignedString(Double.doubleToRawLongBits(d),16)).collect(java.util.stream.Collectors.joining(",")); }
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);Class.forName("akt").getMethod("a").invoke(null);
        Class<?> template=Class.forName("ent"), pos=Class.forName("jd"), vec=Class.forName("exc"), integerVec=Class.forName("kh");
        Class<?> mirror=Class.forName("dkv"),rotation=Class.forName("dmm");
        Object[] mirrors=mirror.getEnumConstants(),rotations=rotation.getEnumConstants();
        var pc=pos.getConstructor(int.class,int.class,int.class);
        var vc=vec.getConstructor(double.class,double.class,double.class);
        var ic=integerVec.getConstructor(int.class,int.class,int.class);
        Method blockTransform=template.getMethod("a",pos,mirror,rotation,pos);
        Method entityTransform=template.getMethod("a",vec,mirror,rotation,pos);
        Method bounds=template.getDeclaredMethod("a",pos,rotation,pos,mirror,integerVec);bounds.setAccessible(true);
        Method xp=pos.getMethod("u"),yp=pos.getMethod("v"),zp=pos.getMethod("w");
        Field vx=vec.getField("c"),vy=vec.getField("d"),vz=vec.getField("e");
        int[][] points={{0,0,0},{-3,4,5},{Integer.MIN_VALUE,Integer.MAX_VALUE,-1},{1875000,-64,-1875000}};
        int[][] pivots={{0,0,0},{1,0,1},{-13,9,7},{Integer.MAX_VALUE,0,Integer.MIN_VALUE}};
        int[][] sizes={{1,1,1},{5,7,9},{0,0,0},{17,32,65}};
        int[] origin={-1234,67,890};Object originObject=pc.newInstance(origin[0],origin[1],origin[2]);
        List<String> rows=new ArrayList<>();
        for(int p=0;p<points.length;p++) for(int[] pivot:pivots) for(int m=0;m<3;m++) for(int r=0;r<4;r++) {
            int[] point=points[p],size=sizes[p];Object input=pc.newInstance(point[0],point[1],point[2]);Object pv=pc.newInstance(pivot[0],pivot[1],pivot[2]);
            double[] d={point[0]+0.25,point[1]+0.5,point[2]+0.75};
            Object out=blockTransform.invoke(null,input,mirrors[m],rotations[r],pv);
            Object e=entityTransform.invoke(null,vc.newInstance(d[0],d[1],d[2]),mirrors[m],rotations[r],pv);
            Object box=bounds.invoke(null,originObject,rotations[r],pv,mirrors[m],ic.newInstance(size[0],size[1],size[2]));
            int[] extent=new int[6];int i=0;
            for(String name:new String[]{"h","i","j","k","l","m"}) extent[i++]=(int)box.getClass().getMethod(name).invoke(box);
            rows.add(m+"\t"+r+"\t"+ints(point)+"\t"+ints(pivot)+"\t"+bits(d)+"\t"+ints((int)xp.invoke(out),(int)yp.invoke(out),(int)zp.invoke(out))+"\t"+bits(vx.getDouble(e),vy.getDouble(e),vz.getDouble(e))+"\t"+ints(size)+"\t"+ints(origin)+"\t"+ints(extent));
        }
        Files.write(Path.of(args[0]),rows);
        Class<?> state=Class.forName("dtc");Method mirrorState=state.getMethod("a",mirror),rotateState=state.getMethod("a",rotation);
        List<Object> states=new ArrayList<>();IdentityHashMap<Object,Integer> indices=new IdentityHashMap<>();
        for(Object s:(Iterable<?>)Class.forName("dfy").getField("q").get(null)) { indices.put(s,states.size());states.add(s); }
        rows.clear();
        for(Object s:states) {
            StringBuilder row=new StringBuilder(s.toString());
            for(Object m:mirrors) for(Object r:rotations) {
                Object transformed=rotateState.invoke(mirrorState.invoke(s,m),r);
                row.append('\t').append(Objects.requireNonNull(indices.get(transformed)));
            }
            rows.add(row.toString());
        }
        Files.write(Path.of(args[1]),rows);
        System.out.println("native coordinate/bounds cases: 192; native state transforms: "+states.size()*12);
    }
}
