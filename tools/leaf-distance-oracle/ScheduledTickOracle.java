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
    static Entry unpacked(Object value) throws Exception {
        Class<?> tick=Class.forName("eyt"),pos=Class.forName("jd");
        Object p=tick.getMethod("b").invoke(value);
        return new Entry((String)tick.getMethod("a").invoke(value),
            (int)pos.getMethod("u").invoke(p),(int)pos.getMethod("v").invoke(p),(int)pos.getMethod("w").invoke(p),
            (long)tick.getMethod("c").invoke(value),((Enum<?>)tick.getMethod("d").invoke(value)).name(),
            (long)tick.getMethod("e").invoke(value));
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
        if(args.length<2)return;
        Class<?> proto=Class.forName("eyr");
        Method stage=proto.getMethod("a",tick),pending=proto.getMethod("b"),unpack=container.getMethod("a",long.class);
        @SuppressWarnings({"unchecked","rawtypes"})
        Object normal=Enum.valueOf((Class<Enum>)priority,"NORMAL");
        List<String> protoRows=new ArrayList<>();
        for(int scenario=0;scenario<18;scenario++) {
            long now=100+scenario;int limit=scenario==0?65536:List.of(1,5,65536).get(scenario%3);
            List<Entry> generated=new ArrayList<>(),before=new ArrayList<>(),after=new ArrayList<>();
            if(scenario==0)for(int x:new int[]{0,16,17,18})generated.add(new Entry("W",x,70,0,0,"NORMAL",1000+generated.size()));
            else if(scenario!=1) {
                List<int[]> chunks=new ArrayList<>();
                if(scenario<12)chunks.addAll(List.of(new int[]{0,0},new int[]{-1,0},new int[]{1,1}));
                else for(int cx=-3;cx<=3;cx++)for(int cz=-3;cz<=3;cz++)chunks.add(new int[]{cx,cz});
                for(int i=0;i<chunks.size();i++) {
                    int[] ch=chunks.get(i);int count=1+(i*7+scenario)%7;
                    for(int j=0;j<count;j++)generated.add(new Entry((j%3==0 && scenario%2==0)?"L":"W",ch[0]*16+j%4,70+j/4,ch[1]*16,0,"NORMAL",1000+generated.size()));
                }
                if(scenario==6)generated.add(new Entry("W",generated.get(0).x(),generated.get(0).y(),generated.get(0).z(),0,"NORMAL",4000));
                Collections.shuffle(generated,new Random(60007+scenario));
            }
            if(scenario>0) {
                if(scenario%2==0)for(int i=0;i<2;i++)before.add(new Entry("W",1280+i,70,0,now,"NORMAL",i));
                for(int i=0;i<2;i++)after.add(new Entry("L",1282+i,70,0,now,"NORMAL",before.size()+i));
            }
            Set<Long> cold=new HashSet<>();
            if(scenario>1 && scenario%4==0)cold.add((1L & 0xffffffffL) | (1L<<32));
            List<int[]> coldPoints=cold.isEmpty()?List.of():List.of(new int[]{1,1});
            Object queue=level.getConstructor(LongPredicate.class,Supplier.class).newInstance((LongPredicate)key->!cold.contains(key),(Supplier<Object>)()->profiler);
            Object live=container.getConstructor().newInstance();add.invoke(queue,cc.newInstance(80,0),live);
            for(Entry e:before)schedule.invoke(queue,tc.newInstance(e.type().intern(),pc.newInstance(e.x(),e.y(),e.z()),e.due(),normal,e.order()));
            Map<String,Object> staged=new LinkedHashMap<>();
            for(Entry e:generated) {
                String key=(e.x()>>4)+","+(e.z()>>4);
                if(!staged.containsKey(key))staged.put(key,proto.getConstructor().newInstance());
                stage.invoke(staged.get(key),tc.newInstance(e.type().intern(),pc.newInstance(e.x(),e.y(),e.z()),e.due(),normal,e.order()));
            }
            List<Object> loaded=new ArrayList<>();loaded.add(live);
            for(var entry:staged.entrySet()) {
                List<?> saved=(List<?>)pending.invoke(entry.getValue());
                for(Object s:saved)if((int)Class.forName("eys").getMethod("c").invoke(s)!=0 || Class.forName("eys").getMethod("d").invoke(s)!=normal)
                    throw new AssertionError("generated queue is not zero-delay normal priority");
                Object active=container.getConstructor(List.class).newInstance(saved);unpack.invoke(active,now);
                String[] ch=entry.getKey().split(",");add.invoke(queue,cc.newInstance(Integer.parseInt(ch[0]),Integer.parseInt(ch[1])),active);loaded.add(active);
            }
            for(Entry e:after)schedule.invoke(queue,tc.newInstance(e.type().intern(),pc.newInstance(e.x(),e.y(),e.z()),e.due(),normal,e.order()));
            Map<String,Entry> metadata=new HashMap<>();
            for(Object active:loaded)for(Object value:((java.util.stream.Stream<?>)container.getMethod("d").invoke(active)).toList()) {
                Entry e=unpacked(value);metadata.put(e.type()+","+e.x()+","+e.y()+","+e.z(),e);
            }
            List<Entry> delivered=new ArrayList<>();
            BiConsumer<Object,Object> callback=(p,t)->{try {
                String key=t+","+xp.invoke(p)+","+yp.invoke(p)+","+zp.invoke(p);
                Entry e=metadata.get(key);if(e==null)throw new AssertionError("unobserved native tick "+key);delivered.add(e);
            }catch(ReflectiveOperationException e){throw new IllegalStateException(e);}};
            run.invoke(queue,now,limit,callback);String first=events(delivered);delivered.clear();cold.clear();
            run.invoke(queue,now+1,limit,callback);
            protoRows.add(scenario+"\t"+now+"\t"+limit+"\t"+points(coldPoints)+"\t"+events(before)+"\t"+events(generated)+"\t"+events(after)+"\t"+first+"|"+events(delivered));
        }
        Files.write(Path.of(args[1]),protoRows);
        System.out.println("native proto-unpack callback cases: "+protoRows.size());
    }
}
