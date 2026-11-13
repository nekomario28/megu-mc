package dev.mcspine.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.*;
import java.util.stream.Stream;

/** Real createStructures and actual piece/start methods. Scene inputs below are
 * authored formulas, not a saved world, captured heightmap or saved piece NBT.
 */
public final class MineshaftPipelineOracle {
    static Class<?> c(String s){return NativeHarness.C(s);}
    static Object f(String cl,String n)throws Exception{return c(cl).getField(n).get(null);}
    static Object registry(Object a,String n)throws Exception{return c("ka").getMethod("d",c("akq")).invoke(a,f("lu",n));}
    static Object value(Object r,String n)throws Exception{
        Object id=c("akr").getMethod("a",String.class).invoke(null,n);
        Object v=c("jz").getMethod("a",c("akr")).invoke(r,id);
        if(v==null)throw new AssertionError("missing "+n);return v;
    }
    static Object holder(Object v)throws Exception{return c("jm").getMethod("a",Object.class).invoke(null,v);}
    static String name(Object r,Object v)throws Exception{return c("jz").getMethod("b",Object.class).invoke(r,v).toString();}
    static Object chunk(int x,int z,Object biomes)throws Exception{
        return c("dvs").getConstructor(c("dcd"),c("dvv"),c("dcy"),c("jz"),c("eab")).newInstance(
            c("dcd").getConstructor(int.class,int.class).newInstance(x,z),f("dvv","a"),
            c("dcy").getMethod("e",int.class,int.class).invoke(null,-64,384),biomes,null);
    }
    static void bindTags(Object biomes,Object manager)throws Exception{
        Function<Object,Object> lookup=id->{try{return c("jz").getMethod("c",c("akr")).invoke(biomes,id);}catch(Exception e){throw new AssertionError(e);}};
        Object loader=c("awv").getConstructor(Function.class,String.class).newInstance(lookup,
            c("lu").getMethod("d",c("akq")).invoke(null,f("lu","aF")));
        Map<?,?> loaded=(Map<?,?>)c("awv").getMethod("b",c("aue")).invoke(loader,manager);
        if(loaded.isEmpty())throw new AssertionError("missing native biome tags");
        Map<Object,Object> tags=new HashMap<>();
        for(var e:loaded.entrySet())tags.put(c("awu").getMethod("a",c("akq"),c("akr")).invoke(null,f("lu","aF"),e.getKey()),List.copyOf((Collection<?>)e.getValue()));
        c("jz").getMethod("a",Map.class).invoke(biomes,tags);
    }
    static List<?> pieces(Object builder)throws Exception{
        Field p=c("ekj").getDeclaredField("a");p.setAccessible(true);return (List<?>)p.get(builder);
    }
    static Object context(Object access,Object generator,Object source,Object random,long seed,int x,int z)throws Exception{
        return c("ejr$a").getConstructor(c("ka"),c("duz"),c("dea"),c("dzm"),c("enu"),long.class,c("dcd"),c("dcy"),Predicate.class).newInstance(
            access,generator,source,random,null,seed,c("dcd").getConstructor(int.class,int.class).newInstance(x,z),
            c("dcy").getMethod("e",int.class,int.class).invoke(null,-64,384),(Predicate<Object>)h->true);
    }
    static JsonArray rngState(Object rng)throws Exception{
        Field source=c("dzx").getDeclaredField("d");source.setAccessible(true);Object xor=source.get(rng);
        if(xor.getClass()!=c("dzz"))throw new AssertionError("wrong placement source");
        Field core=c("dzz").getDeclaredField("e");core.setAccessible(true);Object bits=core.get(xor);
        JsonArray out=new JsonArray();for(String n:new String[]{"b","c"}){Field v=c("dzy").getDeclaredField(n);v.setAccessible(true);out.add(Long.toString(v.getLong(bits)));}return out;
    }
    static JsonObject writer(Object generator,Object sm,Object biomeRegistry,Object biome,Object builder,Object start,
                             long seed,int sx,int sz,int child,int cx,int cz,String variant,String scene,int top,int first)throws Exception{
        List<?> ps=builder==null?List.of():pieces(builder);
        Object selected=child<0?null:ps.get(child);
        int cavern=child<0?21:MineshaftLayoutOracle.tag(selected).getAsJsonObject().getAsJsonArray("BB").get(1).getAsInt()-1;
        NativeHarness.Level level=NativeHarness.level((x,y,z)->y<top&&(scene.equals("suspended")?y>=cavern+6:scene.equals("solid")||y!=cavern)?"minecraft:stone":"minecraft:air");
        JsonArray events=new JsonArray(),heights=new JsonArray(),entities=new JsonArray();
        Map<List<Integer>,Object> blockEntities=new HashMap<>();
        Map<Long,Object> chunks=new HashMap<>();
        Object world=Proxy.newProxyInstance(MineshaftPipelineOracle.class.getClassLoader(),new Class<?>[]{c("dds")},(self,m,a)->{
            Class<?>[] p=m.getParameterTypes();
            // LevelReader overrides these defaults through dimensionType. The
            // authored level instead has the explicit Overworld height accessor.
            if(p.length==0&&m.getName().equals("I_"))return NativeHarness.MIN_Y;
            if(p.length==0&&m.getName().equals("J_"))return NativeHarness.HEIGHT;
            if(m.getName().equals("t")&&Arrays.equals(p,new Class<?>[]{c("jd")}))return biome;
            if(m.getName().equals("c_")&&Arrays.equals(p,new Class<?>[]{c("jd")})){
                int[] at=NativeHarness.xyz(a[0]);return blockEntities.get(List.of(at[0],at[1],at[2]));
            }
            // Constructors/loot setters work without a tickable Level. This arm
            // captures actual addFreshEntity; it does not simulate vehicle motion.
            if(m.getName().equals("E")&&p.length==0)return null;
            if(m.getName().equals("b")&&Arrays.equals(p,new Class<?>[]{c("bsr")})){
                Object entity=a[0];if(!c("cpa").isInstance(entity))throw new AssertionError("unexpected entity "+entity);
                JsonArray data=new JsonArray();for(String method:new String[]{"dt","dv","dz"})data.add((double)c("bsr").getMethod(method).invoke(entity));
                Object key=c("cou").getMethod("B").invoke(entity);
                data.add(c("akq").getMethod("a").invoke(key).toString());
                data.add(Long.toString((long)c("cou").getMethod("C").invoke(entity)));entities.add(data);return true;
            }
            if(m.getName().equals("a")&&Arrays.equals(p,new Class<?>[]{int.class,int.class,c("dvz"),boolean.class})&&m.getReturnType()==c("duy")){
                long key=((long)(int)a[0]&0xffffffffL)|((long)(int)a[1]<<32);
                Object target=chunks.get(key);if(target==null){target=chunk((int)a[0],(int)a[1],biomeRegistry);chunks.put(key,target);}return target;
            }
            if(m.getName().equals("a")&&Arrays.equals(p,new Class<?>[]{c("dyy$a"),int.class,int.class})){
                if(a[0]!=f("dyy$a","c"))throw new AssertionError("wrong WG type "+a[0]);
                JsonArray h=new JsonArray();h.add((int)a[1]);h.add((int)a[2]);h.add(first);heights.add(h);return first;
            }
            if(m.getName().equals("a")&&Arrays.equals(p,new Class<?>[]{c("jd"),c("dtc"),int.class})){
                int[] xyz=NativeHarness.xyz(a[0]);JsonArray event=new JsonArray();for(int n:xyz)event.add(n);
                String stateName=NativeHarness.normalizedStateName(a[1]);event.add(stateName);events.add(event);
                List<Integer> key=List.of(xyz[0],xyz[1],xyz[2]);
                if(stateName.equals("minecraft:spawner"))blockEntities.put(key,c("drw").getConstructor(c("jd"),c("dtc")).newInstance(a[0],a[1]));
                else blockEntities.remove(key);
            }
            if(m.isDefault())return InvocationHandler.invokeDefault(self,m,a);
            try{return m.invoke(level.proxy,a);}catch(InvocationTargetException e){throw e.getCause();}
        });
        Object rng=c("dzx").getConstructor(c("ayw")).newInstance(c("dzz").getConstructor(long.class).newInstance(0L));
        long population=(long)c("dzx").getMethod("a",long.class,int.class,int.class).invoke(rng,seed,cx<<4,cz<<4);
        int slot=variant.equals("mesa")?2:1;
        c("dzx").getMethod("b",long.class,int.class,int.class).invoke(rng,population,slot,3);
        JsonArray before=rngState(rng),startRng=new JsonArray(),startWrites=new JsonArray();
        Object clip=c("ejj").getConstructor(int.class,int.class,int.class,int.class,int.class,int.class).newInstance(cx<<4,-63,cz<<4,(cx<<4)+15,319,(cz<<4)+15);
        Object pos=c("dcd").getConstructor(int.class,int.class).newInstance(cx,cz);
        if(child<0){
            List<?> starts=start instanceof List?(List<?>)start:List.of(start);
            for(Object actualStart:starts){int count=events.size();
                c("ejz").getMethod("a",c("dds"),c("ddq"),c("duz"),c("ayw"),c("ejj"),c("dcd")).invoke(actualStart,world,sm,generator,rng,clip,pos);
                startRng.add(rngState(rng));startWrites.add(events.size()-count);
            }
        }
        else c("ejv").getMethod("a",c("dds"),c("ddq"),c("duz"),c("ayw"),c("ejj"),c("dcd"),c("jd")).invoke(selected,world,sm,generator,rng,clip,pos,NativeHarness.pos(cx<<4,0,cz<<4));
        for(String log:level.log)if(log.startsWith("unsupported"))throw new AssertionError(log);
        JsonObject row=new JsonObject();row.addProperty("seed",Long.toString(seed));row.addProperty("sx",sx);row.addProperty("sz",sz);
        row.addProperty("child",child);row.addProperty("cx",cx);row.addProperty("cz",cz);row.addProperty("variant",variant);
        row.addProperty("cavern",cavern);row.addProperty("first_available",first);row.addProperty("terrain_top",top);row.addProperty("scene",scene);row.addProperty("slot",slot);row.addProperty("population_seed",Long.toString(population));
        row.add("rng_before",before);row.add("rng_after",rngState(rng));row.add("height_calls",heights);row.add("events",events);row.add("entities",entities);row.add("per_start_rng",startRng);row.add("start_write_counts",startWrites);
        return row;
    }
    static long packed(int x,int z){return ((long)x&0xffffffffL)|((long)z<<32);}
    static JsonObject multi(Object access,Object generator,Object source,Object settings,Object noises,Object filtered,Object biomes,Object structure)throws Exception{
        long seed=4242L;
        Object random=c("dzm").getMethod("a",c("dzd"),c("jn"),long.class).invoke(null,settings,noises,seed);
        Object state=c("dva").getMethod("a",c("dzm"),long.class,c("dea"),c("jo")).invoke(null,random,seed,source,filtered);
        Object emptyManager=c("ddq").getConstructor(c("dcx"),c("dzw"),c("ejs")).newInstance(null,null,null);
        List<Set<Long>> footprints=new ArrayList<>();
        for(int[] origin:new int[][]{{-4,-3},{4,3},{9,-1}}){
            Object target=chunk(origin[0],origin[1],biomes);
            c("duz").getMethod("a",c("ka"),c("dva"),c("ddq"),c("duy"),c("enu")).invoke(generator,access,state,emptyManager,target,null);
            Object start=c("dvu").getMethod("a",c("ejr")).invoke(target,structure);
            if(start==null||!(boolean)c("ejz").getMethod("b").invoke(start))throw new AssertionError("multi origin not actually admitted");
            Set<Long> footprint=new TreeSet<>();
            for(Object piece:(List<?>)c("ejz").getMethod("i").invoke(start)){
                JsonArray bb=MineshaftLayoutOracle.tag(piece).getAsJsonObject().getAsJsonArray("BB");
                for(int x=bb.get(0).getAsInt()>>4;x<=bb.get(3).getAsInt()>>4;x++)for(int z=bb.get(2).getAsInt()>>4;z<=bb.get(5).getAsInt()>>4;z++)footprint.add(packed(x,z));
            }
            footprints.add(footprint);
        }
        Long selected=null;
        for(int a=0;a<footprints.size()&&selected==null;a++)for(int b=a+1;b<footprints.size()&&selected==null;b++)for(long key:footprints.get(a))if(footprints.get(b).contains(key)){selected=key;break;}
        if(selected==null)throw new AssertionError("no two-start piece footprint overlap");
        int cx=(int)(long)selected,cz=(int)((long)selected>>32);
        Map<Long,Object> chunks=new HashMap<>();Map<Object,Long> origins=new IdentityHashMap<>();
        for(int x=cx-8;x<=cx+8;x++)for(int z=cz-8;z<=cz+8;z++){
            Object target=chunk(x,z,biomes);chunks.put(packed(x,z),target);
            c("duz").getMethod("a",c("ka"),c("dva"),c("ddq"),c("duy"),c("enu")).invoke(generator,access,state,emptyManager,target,null);
            Object start=c("dvu").getMethod("a",c("ejr")).invoke(target,structure);
            if(start!=null)origins.put(start,packed(x,z));
        }
        Object world=Proxy.newProxyInstance(MineshaftPipelineOracle.class.getClassLoader(),new Class<?>[]{c("dds")},(self,m,a)->{
            if(m.getName().equals("I_")&&m.getParameterCount()==0)return -64;
            if(m.getName().equals("J_")&&m.getParameterCount()==0)return 384;
            if(m.getName().equals("E")&&m.getParameterCount()==0)return null;
            if(m.getName().equals("H_")&&m.getParameterCount()==0)return access;
            Class<?>[] p=m.getParameterTypes();
            if(m.getName().equals("a")&&p.length>=2&&p[0]==int.class&&p[1]==int.class&&c("duy").isAssignableFrom(m.getReturnType())){
                Object target=chunks.get(packed((int)a[0],(int)a[1]));if(target==null)throw new AssertionError("unserved reference chunk "+Arrays.toString(a));return target;
            }
            if(m.isDefault())return InvocationHandler.invokeDefault(self,m,a);
            throw new AssertionError("unserved reference method "+m);
        });
        Object sm=c("ddq").getConstructor(c("dcx"),c("dzw"),c("ejs")).newInstance(world,null,null);
        Object target=chunks.get(selected);
        c("duz").getMethod("a",c("dds"),c("ddq"),c("duy")).invoke(generator,world,sm,target);
        Object section=c("kf").getMethod("a",c("duy")).invoke(null,target);
        List<?> starts=(List<?>)c("ddq").getMethod("a",c("kf"),c("ejr")).invoke(sm,section,structure);
        if(starts.size()<2)throw new AssertionError("native manager did not expose multiple starts");
        JsonArray order=new JsonArray();for(Object start:starts)order.add(Long.toString(origins.get(start)));
        JsonObject row=writer(generator,sm,biomes,holder(value(biomes,"minecraft:plains")),null,starts,seed,0,0,-2,cx,cz,"normal","solid",80,80);
        int writers=0;for(JsonElement n:row.getAsJsonArray("start_write_counts"))if(n.getAsInt()>0)writers++;
        if(writers<2)throw new AssertionError("multiple starts did not both write");
        row.add("reference_order",order);return row;
    }
    public static void main(String[] args)throws Exception{
        NativeHarness.bootstrap();
        Object access=c("ka").getMethod("a",c("jz")).invoke(null,f("lt","aA"));
        Object manager=c("atx").getConstructor(c("ass"),List.class).newInstance(f("ass","b"),List.of(c("ats").getMethod("b").invoke(null)));
        access=c("akm").getMethod("a",c("aue"),c("ka"),List.class).invoke(null,manager,access,f("akm","a"));
        Object biomes=registry(access,"aF"),structures=registry(access,"aR"),sets=registry(access,"aT");
        if(args.length<2||!args[1].equals("missing-tags-control"))bindTags(biomes,manager);
        JsonObject out=new JsonObject();JsonArray slots=new JsonArray();Map<Integer,Integer> indices=new HashMap<>();
        for(Object s:((Stream<?>)c("jz").getMethod("t").invoke(structures)).toList()){
            int st=((Enum<?>)c("ejr").getMethod("c").invoke(s)).ordinal(),index=indices.getOrDefault(st,0);indices.put(st,index+1);
            JsonArray row=new JsonArray();row.add(name(structures,s));row.add(st);row.add(index);slots.add(row);
        }
        JsonObject memberships=new JsonObject();
        for(String id:new String[]{"minecraft:mineshaft","minecraft:mineshaft_mesa"}){
            Object allowed=c("ejr").getMethod("a").invoke(value(structures,id));
            List<?> members=((Stream<?>)c("jq").getMethod("a").invoke(allowed)).toList();
            if(members.isEmpty())throw new AssertionError("UNBOUND_MINESHAFT_BIOME_TAG "+id);
            List<String> names=new ArrayList<>();for(Object h:members)names.add(name(biomes,c("jm").getMethod("a").invoke(h)));Collections.sort(names);
            memberships.add(id,new Gson().toJsonTree(names));
        }
        Object set=value(sets,"minecraft:mineshafts"),lookup=c("jz").getMethod("q").invoke(sets);
        Object filtered=c("jo$b").getMethod("a",Predicate.class).invoke(lookup,(Predicate<Object>)v->v==set);
        Object placement=c("ejx").getMethod("b").invoke(set),settings=value(registry(access,"aO"),"minecraft:overworld");
        Object source=c("dej").getMethod("a",c("jm")).invoke(null,holder(value(registry(access,"aZ"),"minecraft:overworld")));
        Object generator=c("dzb").getConstructor(c("dea"),c("jm")).newInstance(source,holder(settings));
        Object noises=c("jz").getMethod("q").invoke(registry(access,"aP"));
        Object sm=c("ddq").getConstructor(c("dcx"),c("dzw"),c("ejs")).newInstance(null,null,null);
        JsonArray cases=new JsonArray(),writers=new JsonArray(),mesas=new JsonArray();
        for(long seed:new long[]{4242L,0L,1L,-1L,0x123456789abcdef0L,Long.MAX_VALUE}){
            Object random=c("dzm").getMethod("a",c("dzd"),c("jn"),long.class).invoke(null,settings,noises,seed);
            Object state=c("dva").getMethod("a",c("dzm"),long.class,c("dea"),c("jo")).invoke(null,random,seed,source,filtered);
            JsonObject run=new JsonObject();run.addProperty("seed",Long.toString(seed));JsonArray starts=new JsonArray(),freq=new JsonArray();
            for(int x=-5;x<=11;x++)for(int z=-10;z<=6;z++){
                boolean eligible=(boolean)c("eko").getMethod("b",c("dva"),int.class,int.class).invoke(placement,state,x,z);
                JsonArray trial=new JsonArray();trial.add(x);trial.add(z);trial.add(eligible);freq.add(trial);
                Object proto=chunk(x,z,biomes);
                c("duz").getMethod("a",c("ka"),c("dva"),c("ddq"),c("duy"),c("enu")).invoke(generator,access,state,sm,proto,null);
                for(String id:new String[]{"minecraft:mineshaft","minecraft:mineshaft_mesa"}){
                    Object structure=value(structures,id),start=c("dvu").getMethod("a",c("ejr")).invoke(proto,structure);
                    if(start==null||!(boolean)c("ejz").getMethod("b").invoke(start))continue;
                    if(!eligible)throw new AssertionError("noncandidate start");
                    JsonArray admitted=new JsonArray();admitted.add(x);admitted.add(z);admitted.add(id);
                    admitted.add(((List<?>)c("ejz").getMethod("i").invoke(start)).size());starts.add(admitted);
                    if(seed==4242L&&x==4&&z==3){
                        Object ctx=context(access,generator,source,random,seed,x,z);
                        Object stub=((Optional<?>)c("elw").getMethod("a",c("ejr$a")).invoke(structure,ctx)).orElseThrow();
                        Object builder=c("ejr$b").getMethod("a").invoke(stub);
                        writers.add(writer(generator,sm,biomes,holder(value(biomes,"minecraft:snowy_plains")),builder,start,seed,x,z,-1,3,-2,"normal","floor_gap",80,80));
                        writers.add(writer(generator,sm,biomes,holder(value(biomes,"minecraft:snowy_plains")),builder,start,seed,x,z,-1,3,-2,"normal","floor_gap",80,22));
                    }
                }
            }
            run.add("frequency_trials",freq);run.add("accepted_starts",starts);cases.add(run);
            // Force Mesa findGenerationPoint, independent of eligibility, to pin
            // terrain queries and the otherwise untested height-adjustment arm.
            int sx=4,sz=3;
            Object structure=value(structures,"minecraft:mineshaft_mesa"),ctx=context(access,generator,source,random,seed,sx,sz);
            Object stub=((Optional<?>)c("elw").getMethod("a",c("ejr$a")).invoke(structure,ctx)).orElseThrow();
            Object builder=c("ejr$b").getMethod("a").invoke(stub);
            JsonArray pp=MineshaftLayoutOracle.pieces(builder);
            Object partition=context(access,generator,source,random,seed,sx,sz),rng=c("ejr$a").getMethod("f").invoke(partition);
            c("ayw").getMethod("j").invoke(rng);
            Object room=c("elv$d").getConstructor(int.class,c("ayw"),int.class,int.class,c("elw$a")).newInstance(0,rng,(sx<<4)+2,(sz<<4)+2,f("elw$a","b"));
            Object parts=c("ekj").getConstructor().newInstance();c("ekj").getMethod("a",c("ejv")).invoke(parts,room);
            c("elv$d").getMethod("a",c("ejv"),c("ejw"),c("ayw")).invoke(room,room,parts,rng);
            Object bb=c("ekj").getMethod("d").invoke(parts),centre=c("ejj").getMethod("g").invoke(bb);
            int[] xyz=NativeHarness.xyz(centre);
            int height=(int)c("duz").getMethod("a",int.class,int.class,c("dyy$a"),c("dcy"),c("dzm")).invoke(generator,xyz[0],xyz[2],f("dyy$a","a"),c("dcy").getMethod("e",int.class,int.class).invoke(null,-64,384),random);
            int target=height<=63?63:(int)c("ayo").getMethod("b",c("ayw"),int.class,int.class).invoke(null,rng,63,height);
            int shift=target-xyz[1];c("ekj").getMethod("a",int.class).invoke(parts,shift);
            if(!pp.equals(MineshaftLayoutOracle.pieces(parts)) ||
               !MineshaftLayoutOracle.checkpoint(rng).equals(MineshaftLayoutOracle.checkpoint(c("ejr$a").getMethod("f").invoke(ctx))))
                throw new AssertionError("Mesa partition changed pieces or RNG");
            JsonObject mesa=new JsonObject();mesa.addProperty("seed",Long.toString(seed));mesa.addProperty("sx",sx);mesa.addProperty("sz",sz);
            mesa.addProperty("x",xyz[0]);mesa.addProperty("z",xyz[2]);mesa.addProperty("height",height);mesa.addProperty("shift",shift);
            mesa.addProperty("count",pp.size());mesa.addProperty("rng_state48",MineshaftLayoutOracle.checkpoint(rng).get("state48").getAsString());
            mesas.add(mesa);
            if(seed==4242L){for(int i=0;i<pp.size();i++){
                JsonObject p=pp.get(i).getAsJsonObject();
                if(!p.get("id").getAsString().equals("minecraft:mscorridor")||p.get("sc").getAsInt()!=0)continue;
                JsonArray bounds=p.getAsJsonArray("BB");
                writers.add(writer(generator,sm,biomes,holder(value(biomes,"minecraft:snowy_plains")),builder,null,seed,sx,sz,i,bounds.get(0).getAsInt()>>4,bounds.get(2).getAsInt()>>4,"mesa","floor_gap",200,200));break;
            }}
        }
        // Controlled biome-source intervention, not a claim about a default
        // Overworld Mesa location. Use the real FixedBiomeSource and the same
        // noise generator; normal-first rejection must retry the Mesa entry.
        JsonArray naturalMesas=new JsonArray();
        for(long searchSeed=0;searchSeed<8&&naturalMesas.size()<2;searchSeed++){
            Object nr=c("dzm").getMethod("a",c("dzd"),c("jn"),long.class).invoke(null,settings,noises,searchSeed);
            Object ns=c("dva").getMethod("a",c("dzm"),long.class,c("dea"),c("jo")).invoke(null,nr,searchSeed,source,filtered);
            boolean found=false;
            for(int x=-192;x<=192&&!found;x++)for(int z=-192;z<=192&&!found;z++){
                if(!(boolean)c("eko").getMethod("b",c("dva"),int.class,int.class).invoke(placement,ns,x,z))continue;
                Object target=chunk(x,z,biomes);
                c("duz").getMethod("a",c("ka"),c("dva"),c("ddq"),c("duy"),c("enu")).invoke(generator,access,ns,sm,target,null);
                Object start=c("dvu").getMethod("a",c("ejr")).invoke(target,value(structures,"minecraft:mineshaft_mesa"));
                if(start==null||!(boolean)c("ejz").getMethod("b").invoke(start))continue;
                JsonArray row=new JsonArray();row.add(Long.toString(searchSeed));row.add(x);row.add(z);
                List<?> admittedPieces=(List<?>)c("ejz").getMethod("i").invoke(start);
                row.add(admittedPieces.size());JsonArray tags=new JsonArray();
                for(Object piece:admittedPieces)tags.add(MineshaftLayoutOracle.tag(piece));
                row.add(tags);naturalMesas.add(row);found=true;
            }
        }
        if(naturalMesas.size()<2)throw new AssertionError("natural Overworld Mesa coverage missing");
        out.addProperty("natural_mesa_biome_source","minecraft:multi_noise/overworld");out.add("natural_mesas",naturalMesas);
        JsonArray mesaAdmissions=new JsonArray();
        Object badlandsId=c("akr").getMethod("a",String.class).invoke(null,"minecraft:badlands");
        Object badlands=((Optional<?>)c("jz").getMethod("c",c("akr")).invoke(biomes,badlandsId)).orElseThrow();
        Object fixed=c("deh").getConstructor(c("jm")).newInstance(badlands);
        Object fixedGenerator=c("dzb").getConstructor(c("dea"),c("jm")).newInstance(fixed,holder(settings));
        Object mesaRandom=c("dzm").getMethod("a",c("dzd"),c("jn"),long.class).invoke(null,settings,noises,4242L);
        Object mesaState=c("dva").getMethod("a",c("dzm"),long.class,c("dea"),c("jo")).invoke(null,mesaRandom,4242L,fixed,filtered);
        Object mesaType=value(structures,"minecraft:mineshaft_mesa");
        for(int[] coord:new int[][]{{-4,-3},{4,3},{9,-1}}){
            int x=coord[0],z=coord[1];Object proto=chunk(x,z,biomes);
            c("duz").getMethod("a",c("ka"),c("dva"),c("ddq"),c("duy"),c("enu")).invoke(fixedGenerator,access,mesaState,sm,proto,null);
            Object start=c("dvu").getMethod("a",c("ejr")).invoke(proto,mesaType);
            if(start==null||!(boolean)c("ejz").getMethod("b").invoke(start))throw new AssertionError("fixed-biome Mesa admission missing");
            List<?> psMesa=(List<?>)c("ejz").getMethod("i").invoke(start);
            JsonArray row=new JsonArray();row.add("4242");row.add(x);row.add(z);row.add("minecraft:mineshaft_mesa");row.add(psMesa.size());
            row.add(MineshaftLayoutOracle.tag(psMesa.get(0)).getAsJsonObject().getAsJsonArray("BB").get(1));
            mesaAdmissions.add(row);
        }
        out.addProperty("mesa_admission_biome_source","fixed:minecraft:badlands");
        // Authored piece-only scenes cover room, crossing, stairs and every
        // corridor orientation/rails/spider flag, with actual spawner entities.
        Object random=c("dzm").getMethod("a",c("dzd"),c("jn"),long.class).invoke(null,settings,noises,0L);
        Object structure=value(structures,"minecraft:mineshaft"),ctx=context(access,generator,source,random,0L,0,0);
        Object stub=((Optional<?>)c("elw").getMethod("a",c("ejr$a")).invoke(structure,ctx)).orElseThrow(),builder=c("ejr$b").getMethod("a").invoke(stub);
        Set<String> covered=new HashSet<>();List<?> ps=pieces(builder);boolean suspended=false;
        for(int i=0;i<ps.size();i++){
            JsonObject p=MineshaftLayoutOracle.tag(ps.get(i)).getAsJsonObject();String kind=p.get("id").getAsString();
            String key=kind.equals("minecraft:mscorridor")?kind+":"+p.get("O")+":"+p.get("hr")+":"+p.get("sc"):kind;
            if(!covered.add(key))continue;
            JsonArray bb=p.getAsJsonArray("BB");int cx=bb.get(0).getAsInt()>>4,cz=bb.get(2).getAsInt()>>4;
            writers.add(writer(generator,sm,biomes,holder(value(biomes,"minecraft:snowy_plains")),builder,null,0L,0,0,i,cx,cz,"normal","floor_gap",80,80));
            if(!suspended&&kind.equals("minecraft:mscorridor")&&p.get("sc").getAsInt()==0){
                writers.add(writer(generator,sm,biomes,holder(value(biomes,"minecraft:snowy_plains")),builder,null,0L,0,0,i,cx,cz,"normal","suspended",80,80));suspended=true;
            }
        }
        JsonArray orders=new JsonArray();
        for(int n:new int[]{0,1,2,24,25,49,100}){
            Object setOrder=Class.forName("it.unimi.dsi.fastutil.longs.LongOpenHashSet").getConstructor().newInstance();
            Method add=setOrder.getClass().getMethod("add",long.class),array=setOrder.getClass().getMethod("toLongArray");
            JsonArray input=new JsonArray(),expected=new JsonArray();
            for(int i=0;i<n;i++){long key=(long)(i-12)&0xffffffffL | ((long)(11-i)<<32);if(i==0)key=0;input.add(Long.toString(key));add.invoke(setOrder,key);add.invoke(setOrder,key);}
            for(long key:(long[])array.invoke(setOrder))expected.add(Long.toString(key));JsonArray row=new JsonArray();row.add(input);row.add(expected);orders.add(row);
        }
        // A real caller-seeded corridor with solid substrate must reach
        // addFreshEntity. The search is fixture production only, never Rust input.
        boolean chestCovered=false;
        for(long s=0;s<100&&!chestCovered;s++){
            Object cr=c("dzm").getMethod("a",c("dzd"),c("jn"),long.class).invoke(null,settings,noises,s);
            Object cc=context(access,generator,source,cr,s,0,0);
            Object cs=((Optional<?>)c("elw").getMethod("a",c("ejr$a")).invoke(value(structures,"minecraft:mineshaft"),cc)).orElseThrow();
            Object cb=c("ejr$b").getMethod("a").invoke(cs);List<?> cp=pieces(cb);
            for(int i=0;i<cp.size();i++)if(c("elv$a").isInstance(cp.get(i))){
                JsonArray bb=MineshaftLayoutOracle.tag(cp.get(i)).getAsJsonObject().getAsJsonArray("BB");
                JsonObject row=writer(generator,sm,biomes,holder(value(biomes,"minecraft:plains")),cb,null,s,0,0,i,bb.get(0).getAsInt()>>4,bb.get(2).getAsInt()>>4,"normal","solid",200,200);
                if(!row.getAsJsonArray("entities").isEmpty()){writers.add(row);chestCovered=true;break;}
            }
        }
        if(!chestCovered)throw new AssertionError("no actual chest-minecart call reached");
        out.add("multi_start",multi(access,generator,source,settings,noises,filtered,biomes,value(structures,"minecraft:mineshaft")));
        out.add("slots",slots);out.add("memberships",memberships);out.add("cases",cases);out.add("mesa",mesas);out.add("mesa_admissions",mesaAdmissions);out.add("writers",writers);out.add("reference_orders",orders);
        Files.writeString(Path.of(args[0]),new GsonBuilder().setPrettyPrinting().create().toJson(out)+"\n");
    }
}
