package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual HashSet<BlockPos> iterator/remove sequences, including tree-bin splits. */
public final class LeafQueueOracle {
    static void add(List<String> ops,int x) { ops.add("+"+x+",0,0"); }
    static List<String> plan(int kind) {
        List<String> ops=new ArrayList<>();
        if(kind==0) {
            for(int i=0;i<64;i++) { ops.add("+"+(i%8)*2+","+(i/8)%4+","+i/32);if(i%3==0) ops.add("p"); }
        } else {
            int count=kind==3?12:16;
            for(int i=0;i<count;i++) add(ops,(kind==4?-64:64)*i);
            if(kind==1) {
                for(int i=0;i<12;i++) ops.add("p");
                for(int i=16;i<25;i++) add(ops,64*i);
                add(ops,0);add(ops,0);
            } else {
                for(int i=1;i<=40;i++) add(ops,i);
                for(int i=0;i<12;i++) ops.add("p");
                for(int i=129;i<160;i++) add(ops,i);
            }
        }
        for(int i=0;i<160;i++) ops.add("p");
        return ops;
    }
    public static void write(Path output) throws Exception {
        Class<?> pos=Class.forName("jd");Constructor<?> ctor=pos.getConstructor(int.class,int.class,int.class);
        Method x=pos.getMethod("u"),y=pos.getMethod("v"),z=pos.getMethod("w");
        List<String> rows=new ArrayList<>();
        for(int kind=0;kind<5;kind++) for(int[] origin:List.of(new int[]{0,0,0},new int[]{-1024,70,512},new int[]{60007,16,-60007})) {
            Set<Object> set=new HashSet<>();List<String> pops=new ArrayList<>(),ops=plan(kind);
            for(String op:ops) {
                if(op.equals("p")) {
                    Iterator<Object> it=set.iterator();
                    if(!it.hasNext()) { pops.add("empty");continue; }
                    Object p=it.next();it.remove();
                    pops.add(((int)x.invoke(p)-origin[0])+","+((int)y.invoke(p)-origin[1])+","+((int)z.invoke(p)-origin[2]));
                } else {
                    String[] a=op.substring(1).split(",");
                    set.add(ctor.newInstance(Integer.parseInt(a[0])+origin[0],Integer.parseInt(a[1])+origin[1],Integer.parseInt(a[2])+origin[2]));
                }
            }
            if(!set.isEmpty()) throw new AssertionError("undrained native queue");
            rows.add(kind+"\t"+origin[0]+","+origin[1]+","+origin[2]+"\t"+String.join(";",ops)+"\t"+String.join(";",pops));
        }
        Files.write(output,rows);System.out.println("leaf queue oracle: "+rows.size()+" actual HashSet<BlockPos> operation vectors");
    }
}
