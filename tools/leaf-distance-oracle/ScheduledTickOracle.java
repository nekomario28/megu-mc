package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.*;

/** Exercises actual LevelTicks / LevelChunkTicks with explicit chunk eligibility. */
public final class ScheduledTickOracle {
    record Entry(String type,int x,int y,int z,long due,String priority,long order) {
        public String toString() { return type+","+x+","+y+","+z+","+due+","+priority+","+order; }
    }
    static String points(List<int[]> chunks) {
        return chunks.stream().map(c->c[0]+","+c[1]).collect(java.util.stream.Collectors.joining(";"));
    }
    static String events(List<Entry> entries) {
        return entries.stream().map(Entry::toString).collect(java.util.stream.Collectors.joining(";"));
    }
    public static void main(String[] args) throws Exception {
        Class<?> level=Class.forName("eyq"), container=Class.forName("eyo"), tick=Class.forName("eyt");
        Class<?> pos=Class.forName("jd"), chunk=Class.forName("dcd"), priority=Class.forName("eyx");
        Object profiler=Class.forName("bnc").getField("a").get(null);
        Constructor<?> pc=pos.getConstructor(int.class,int.class,int.class);
        Constructor<?> cc=chunk.getConstructor(int.class,int.class);
        Constructor<?> tc=tick.getConstructor(Object.class,pos,long.class,priority,long.class);
        Method add=level.getMethod("a",chunk,container), schedule=level.getMethod("a",tick);
        Method run=level.getMethod("a",long.class,int.class,BiConsumer.class);
        Method xp=pos.getMethod("u"), yp=pos.getMethod("v"), zp=pos.getMethod("w");
        List<String> rows=new ArrayList<>();
        for(int scenario=0;scenario<48;scenario++) {
            Random random=new Random(8723+scenario);
            List<int[]> chunks=new ArrayList<>();
            if(scenario<24) chunks.addAll(List.of(new int[]{0,0},new int[]{-1,0},new int[]{1,1}));
            else for(int x=-3;x<=3;x++) for(int z=-3;z<=3;z++) chunks.add(new int[]{x,z});
            Collections.shuffle(chunks,random);
            Set<Long> cold=scenario%4==0 ? Set.of((1L & 0xffffffffL) | (1L<<32)) : Set.of();
            LongPredicate eligible=key->!cold.contains(key);
            Object queue=level.getConstructor(LongPredicate.class,Supplier.class).newInstance(eligible,(Supplier<Object>)()->profiler);
            for(int[] c:chunks) add.invoke(queue,cc.newInstance(c[0],c[1]),container.getConstructor().newInstance());
            List<Entry> in=new ArrayList<>();
            for(int i=0;i<(scenario<24?24:140);i++) {
                int[] c=chunks.get(random.nextInt(chunks.size()));
                String ty=i%2==0?"A":"B";
                int x=c[0]*16+random.nextInt(4), z=c[1]*16+random.nextInt(4);
                long due=9+random.nextInt(4), order=random.nextInt(5)-2;
                String pri=List.of("HIGH","NORMAL","LOW").get(random.nextInt(3));
                in.add(new Entry(ty,x,70,z,due,pri,order));
                if(i==4) in.add(new Entry(ty,x,70,z,due-4,"HIGH",-99));
            }
            for(Entry e:in) {
                @SuppressWarnings({"unchecked","rawtypes"})
                Object pr=Enum.valueOf((Class<Enum>)priority,e.priority());
                schedule.invoke(queue,tc.newInstance(e.type().intern(),pc.newInstance(e.x(),e.y(),e.z()),e.due(),pr,e.order()));
            }
            int limit=scenario%3==0?5:100;
            List<String> out=new ArrayList<>();
            int[] scenarioValue={scenario};
            BiConsumer<Object,Object> callback=(p,t)->{
                try {
                    out.add(t+","+xp.invoke(p)+","+yp.invoke(p)+","+zp.invoke(p));
                    if(out.size()==1 && scenarioValue[0]%5==0) {
                        @SuppressWarnings({"unchecked","rawtypes"})
                        Object normal=Enum.valueOf((Class<Enum>)priority,"NORMAL");
                        schedule.invoke(queue,tc.newInstance(t,p,11L,normal,200L));
                    }
                }
                catch(ReflectiveOperationException e) { throw new IllegalStateException(e); }
            };
            run.invoke(queue,10L,limit,callback);
            String first=String.join(";",out);
            out.clear();
            scenarioValue[0]=-1;
            run.invoke(queue,11L,limit,callback);
            rows.add(scenario+"\t"+points(chunks)+"\t"+(cold.isEmpty()?"": "1,1")+"\t"+limit+"\t"+events(in)+"\t"+first+"|"+String.join(";",out));
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println("native scheduled-tick queue cases: "+rows.size());
    }
}
