package dev.mcspine.tools;

import com.google.gson.*;
import com.mojang.serialization.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.function.Function;

/** Actual FossilFeature -> StructureTemplate.placeInWorld on a scripted, write-through level.
 * No fossil selection, geometry, processors or placement decisions are replayed here.
 * Native resources come from the authenticated game's classpath, not the port's output.
 * This is a block/RNG oracle, not a live server/tick/entity/light acceptance run.
 */
public final class FossilOracle {
    static Class<?> c(String name) throws Exception { return Class.forName(name); }
    static String resource(String path) throws Exception {
        try(var in=c("eci").getResourceAsStream("/data/minecraft/"+path)) {
            if(in==null)throw new AssertionError("UNRESOLVED_RESOURCE "+path);
            return new String(in.readAllBytes(),StandardCharsets.UTF_8);
        }
    }
    static String path(String id) {
        if(!id.startsWith("minecraft:"))throw new AssertionError("unsupported fixture namespace "+id);
        return id.substring(10);
    }
    static String digest(String value) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8)));
    }
    static Object allocate(String name) throws Exception {
        Field f=c("sun.misc.Unsafe").getDeclaredField("theUnsafe");f.setAccessible(true);
        Object unsafe=f.get(null);
        return unsafe.getClass().getMethod("allocateInstance",Class.class).invoke(unsafe,c(name));
    }
    static void field(Object object,Class<?> owner,String name,Object value) throws Exception {
        Field f=owner.getDeclaredField(name);f.setAccessible(true);f.set(object,value);
    }
    static Object load(String id) throws Exception {
        Path file=Files.createTempFile("fossil-input-", ".nbt");
        try {
            try(var in=c("ent").getResourceAsStream("/data/minecraft/structure/"+path(id)+".nbt")) {
                if(in==null)throw new AssertionError("UNRESOLVED_TEMPLATE "+id);
                Files.copy(in,file,StandardCopyOption.REPLACE_EXISTING);
            }
            Object root=c("uo").getMethod("a",Path.class,c("uk")).invoke(null,file,c("uk").getMethod("a").invoke(null));
            Object lookup=c("jz").getMethod("q").invoke(c("lt").getField("e").get(null));
            Object template=c("ent").getConstructor().newInstance();
            c("ent").getMethod("a",c("jn"),c("ub")).invoke(template,lookup,root);
            return template;
        } finally { Files.deleteIfExists(file); }
    }
    // Authored sparse templates exercise clipping and independent palette selection.
    // Their block lists represent the already-loaded stage, sorted as load() requires.
    @SuppressWarnings("unchecked")
    static Object synthetic(JsonObject spec) throws Exception {
        Object template=c("ent").getConstructor().newInstance();
        JsonArray size=spec.getAsJsonArray("size");
        field(template,c("ent"),"n",c("kh").getConstructor(int.class,int.class,int.class)
            .newInstance(size.get(0).getAsInt(),size.get(1).getAsInt(),size.get(2).getAsInt()));
        Field palettes=c("ent").getDeclaredField("l");palettes.setAccessible(true);
        List<Object> target=(List<Object>)palettes.get(template);
        Constructor<?> palette=c("ent$a").getDeclaredConstructor(List.class);palette.setAccessible(true);
        for(JsonElement values:spec.getAsJsonArray("palettes")) {
            List<JsonObject> ordered=new ArrayList<>();
            for(JsonElement b:values.getAsJsonArray())ordered.add(b.getAsJsonObject());
            ordered.sort(Comparator.comparingInt((JsonObject b)->b.getAsJsonArray("pos").get(1).getAsInt())
                .thenComparingInt(b->b.getAsJsonArray("pos").get(0).getAsInt())
                .thenComparingInt(b->b.getAsJsonArray("pos").get(2).getAsInt()));
            List<Object> blocks=new ArrayList<>();
            for(JsonObject b:ordered) {
                JsonArray p=b.getAsJsonArray("pos");
                blocks.add(c("ent$c").getConstructor(c("jd"),c("dtc"),c("ub")).newInstance(
                    NativeHarness.pos(p.get(0).getAsInt(),p.get(1).getAsInt(),p.get(2).getAsInt()),
                    NativeHarness.stateFromJson(b.get("state")),null));
            }
            target.add(palette.newInstance(blocks));
        }
        return template;
    }
    static Object serverWorld(JsonObject config,JsonObject test,List<String> selected) throws Exception {
        Map<Object,Object> cache=new HashMap<>() {
            public Object computeIfAbsent(Object key,Function<? super Object,? extends Object> load) {
                if(!containsKey(key))throw new AssertionError("UNRESOLVED_TEMPLATE_LOOKUP "+key);
                selected.add(key.toString());
                return get(key);
            }
        };
        for(String list:new String[]{"fossil_structures","overlay_structures"}) {
            String replacement=list.equals("fossil_structures")?"base_template":"overlay_template";
            for(JsonElement value:config.getAsJsonArray(list)) {
                String id=value.getAsString();Object key=NativeHarness.newId(id);
                if(!cache.containsKey(key))cache.put(key,Optional.of(test.has(replacement)?synthetic(test.getAsJsonObject(replacement)):load(id)));
            }
        }
        Object manager=allocate("enu");field(manager,c("enu"),"f",cache);
        Object server=allocate("apn");field(server,c("net.minecraft.server.MinecraftServer"),"aA",manager);
        Object world=allocate("aqu");field(world,c("aqu"),"J",server);
        if(c("aqu").getMethod("o").invoke(world)!=server || c("net.minecraft.server.MinecraftServer").getMethod("ba").invoke(server)!=manager)
            throw new AssertionError("native manager getter wiring");
        // Prove a missing binding cannot silently synthesize an empty native template.
        try { c("enu").getMethod("a",c("akr")).invoke(manager,NativeHarness.newId("test:unbound"));
            throw new AssertionError("missing-template control was blind");
        } catch(InvocationTargetException e) {
            if(!(e.getCause() instanceof AssertionError) || !e.getCause().getMessage().startsWith("UNRESOLVED_TEMPLATE_LOOKUP"))throw e;
        }
        return world;
    }
    static JsonObject config(JsonObject test) throws Exception {
        JsonObject cfg=JsonParser.parseString(resource("worldgen/configured_feature/"+test.get("config").getAsString()+".json"))
            .getAsJsonObject().getAsJsonObject("config");
        if(test.has("single"))for(String key:new String[]{"fossil_structures","overlay_structures"}) {
            JsonArray one=new JsonArray();one.add(cfg.getAsJsonArray(key).get(0));cfg.add(key,one);
        }
        for(String key:new String[]{"fossil_processors","overlay_processors"}) {
            String id=cfg.get(key).getAsString();
            cfg.add(key,JsonParser.parseString(resource("worldgen/processor_list/"+path(id)+".json")));
        }
        if(test.has("overrides"))for(var e:test.getAsJsonObject("overrides").entrySet())cfg.add(e.getKey(),e.getValue());
        return cfg;
    }
    static String worldName(JsonObject test,int x,int y,int z) {
        String scene=test.get("scene").getAsString();
        if(scene.equals("air") || y>=64)return "minecraft:air";
        if(scene.equals("water"))return "minecraft:water";
        if(scene.equals("lava"))return "minecraft:lava";
        if(scene.equals("half-air") && x<0)return "minecraft:air";
        if(scene.equals("half-water") && x<0)return "minecraft:water";
        if(scene.equals("half-lava") && x<0)return "minecraft:lava";
        if(scene.equals("protected") && Math.floorMod(x*31+y*7+z*13,5)==0)return "minecraft:bedrock";
        if(scene.equals("deepslate"))return "minecraft:deepslate";
        return "minecraft:stone";
    }
    static int height(JsonObject test,int x,int z) {
        return test.has("sloped")?48+Math.floorMod(x*3+z*7,19):64;
    }
    public static void main(String[] args) throws Exception {
        if(args.length<2 || args.length>3)throw new IllegalArgumentException("cases-json output-tsv [proto-order-tsv]");
        NativeHarness.bootstrap();
        Object feature=NativeHarness.feature("fossil");
        if(!feature.getClass().getName().equals("eci"))throw new AssertionError("wrong native feature "+feature.getClass());
        Codec<?> configCodec=(Codec<?>)c("ecj").getField("a").get(null);
        JsonObject control=new JsonObject();control.addProperty("config","fossil_coal");
        JsonObject valid=config(control);
        for(String bad:List.of("empty","unequal","too-many-corners","negative-corners")) {
            JsonObject value=valid.deepCopy();
            if(bad.equals("empty"))value.add("fossil_structures",new JsonArray());
            if(bad.equals("unequal"))value.add("overlay_structures",new JsonArray());
            if(bad.equals("too-many-corners"))value.addProperty("max_empty_corners_allowed",8);
            if(bad.equals("negative-corners"))value.addProperty("max_empty_corners_allowed",-1);
            boolean rejected=false;
            try { NativeHarness.getOrThrow(configCodec.parse(JsonOps.INSTANCE,value)); }
            catch(Exception expected) { rejected=true; }
            if(!rejected)throw new AssertionError("native config accepted "+bad);
            System.out.println("FOSSIL_CODEC_REJECT "+bad);
        }
        for(double[] pair:new double[][]{{4.5,4},{-0.5,0}}) {
            JsonObject value=valid.deepCopy();value.addProperty("max_empty_corners_allowed",pair[0]);
            Object decoded=NativeHarness.getOrThrow(configCodec.parse(JsonOps.INSTANCE,value));
            if(c("ecj").getField("f").getInt(decoded)!=(int)pair[1])throw new AssertionError("native int codec changed");
            System.out.println("FOSSIL_CODEC_INT "+pair[0]+" -> "+(int)pair[1]);
        }
        List<String> output=new ArrayList<>(),protoOutput=new ArrayList<>();
        for(JsonElement element:JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonArray()) {
            JsonObject test=element.getAsJsonObject(),config=config(test);
            List<String> selected=new ArrayList<>();
            Object serverWorld=serverWorld(config,test,selected);
            // Plain JsonOps decodes DIRECT processor holders, avoiding an absent dynamic
            // registry in this standalone harness. The native processor-list codec runs.
            Object cfg=NativeHarness.getOrThrow(((Codec<?>)c("ecj").getField("a").get(null)).parse(JsonOps.INSTANCE,config));
            SortedMap<String,String> inputs=new TreeMap<>();SortedMap<String,Integer> heights=new TreeMap<>();
            NativeHarness.Level level=NativeHarness.level((x,y,z)-> {
                String name=worldName(test,x,y,z);inputs.put(x+","+y+","+z,name);return name;
            });
            Map<String,Object> fluidContainers=new TreeMap<>();
            Function<Object,Object> container=position-> {
                try {
                    int[] v=NativeHarness.xyz(position);String chunk=(v[0]>>4)+","+(v[2]>>4);
                    if(!fluidContainers.containsKey(chunk))fluidContainers.put(chunk,c("eyr").getConstructor().newInstance());
                    return fluidContainers.get(chunk);
                } catch(Exception e) { throw new AssertionError(e); }
            };
            Object fluidTicks=c("eyy").getConstructor(Function.class).newInstance(container);
            long[] subTick={0};
            Object data=Proxy.newProxyInstance(FossilOracle.class.getClassLoader(),new Class<?>[]{c("erb")},(p,m,a)-> {
                if(m.getParameterCount()==0 && m.getReturnType()==long.class && (m.getName().equals("c") || m.getName().equals("d")))return 0L;
                throw new UnsupportedOperationException("unbound level data "+m);
            });
            Object dimension=NativeHarness.getOrThrow(((Codec<?>)c("dwy").getField("h").get(null))
                .parse(JsonOps.INSTANCE,JsonParser.parseString(resource("dimension_type/overworld.json"))));
            Object proxy=Proxy.newProxyInstance(FossilOracle.class.getClassLoader(),new Class<?>[]{c("dds")},(p,m,a)-> {
                if(m.getName().equals("E") && m.getParameterCount()==0)return serverWorld;
                if(m.getParameterCount()==0) {
                    if(m.getName().equals("O"))return fluidTicks;
                    if(m.getName().equals("A_"))return data;
                    if(m.getName().equals("G_"))return subTick[0]++;
                    if(m.getName().equals("D_"))return dimension;
                }
                if(m.getName().equals("a") && Arrays.equals(m.getParameterTypes(),new Class<?>[]{c("dyy$a"),int.class,int.class})) {
                    if(a[0]!=c("dyy$a").getField("c").get(null))throw new AssertionError("wrong heightmap "+a[0]);
                    int x=(int)a[1],z=(int)a[2],value=height(test,x,z);heights.put(x+","+z,value);return value;
                }
                // Defaults must stay bound to this outer level, so nested tick/data
                // requests reach the explicitly bound native containers above.
                // The harness supplies the primitive three-argument setter itself;
                // do not bypass it through dde's default -> abstract four-arg setter.
                boolean set=m.getName().equals("a") && Arrays.equals(m.getParameterTypes(),
                    new Class<?>[]{c("jd"),c("dtc"),int.class});
                if(m.isDefault() && !set)return InvocationHandler.invokeDefault(p,m,a);
                try { return m.invoke(level.proxy,a); } catch(InvocationTargetException e) { throw e.getCause(); }
            });
            JsonArray p=test.getAsJsonArray("origin");Object rawRng=NativeHarness.rng(test.get("seed").getAsLong());
            List<Integer> bounds=new ArrayList<>(), values=new ArrayList<>();int[] floats={0};
            Object rng=Proxy.newProxyInstance(FossilOracle.class.getClassLoader(),new Class<?>[]{c("ayw")},(r,m,a)-> {
                Object value;
                try { value=m.invoke(rawRng,a); } catch(InvocationTargetException e) { throw e.getCause(); }
                if(m.getReturnType()==int.class && Arrays.equals(m.getParameterTypes(),new Class<?>[]{int.class})) {
                    bounds.add((int)a[0]);values.add((int)value);
                }
                if(m.getReturnType()==float.class && m.getParameterCount()==0)floats[0]++;
                return value;
            });
            Object context=c("ecg").getConstructors()[0].newInstance(Optional.empty(),proxy,NativeHarness.probeGenerator(),rng,
                NativeHarness.pos(p.get(0).getAsInt(),p.get(1).getAsInt(),p.get(2).getAsInt()),cfg);
            boolean result=(boolean)feature.getClass().getMethod("a",c("ecg")).invoke(feature,context);
            if(!bounds.subList(0,3).equals(List.of(4,config.getAsJsonArray("fossil_structures").size(),10)) || selected.size()!=2)
                throw new AssertionError("native fossil entry/lookup footprint "+bounds+" "+selected);
            if(test.has("expect_rotation") && ((int)values.get(0)!=test.get("expect_rotation").getAsInt()
                || !selected.get(0).equals(test.get("expect_base").getAsString())))
                throw new AssertionError("coverage seed did not execute requested arm "+test);
            System.out.println("FOSSIL_PATH "+test.get("id").getAsString()+" rotation="+values.get(0)+" templates="+selected+" int_bounds="+bounds+" float_calls="+floats[0]);
            List<String> cells=new ArrayList<>();
            for(var entry:level.writes.entrySet())cells.add(entry.getKey()+" "+NativeHarness.normalizedStateName(entry.getValue())+"\n");
            Collections.sort(cells);
            if("1".equals(System.getenv("FOSSIL_NATIVE_TRACE")))for(String line:cells)
                System.out.print("FOSSIL_CELL "+test.get("id").getAsString()+" "+line);
            StringBuilder writes=new StringBuilder();
            for(String line:level.log)if(line.startsWith("set ")) {
                int y=Integer.parseInt(line.substring(4).split("[ ,]",4)[1]);
                if(y>=level.minY && y<level.minY+level.height)writes.append(line.substring(4)).append('\n');
            }
            Method next=c("ayw").getMethod("a",int.class);
            String probe=next.invoke(rng,1000)+","+next.invoke(rng,1000)+","+next.invoke(rng,1000);
            List<String> pendingFluids=new ArrayList<>();Object fluidRegistry=c("lt").getField("c").get(null);
            SortedMap<String,List<String>> protoOrder=new TreeMap<>();
            for(var chunk:fluidContainers.entrySet())for(Object saved:(List<?>)c("eyr").getMethod("b").invoke(chunk.getValue())) {
                if((int)c("eys").getMethod("c").invoke(saved)!=0 || c("eys").getMethod("d").invoke(saved)!=c("eyx").getField("d").get(null))
                    throw new AssertionError("unexpected proto-fluid timing/priority");
                Object type=c("eys").getMethod("a").invoke(saved),position=c("eys").getMethod("b").invoke(saved);
                int[] v=NativeHarness.xyz(position);
                Object id=c("jz").getMethod("b",Object.class).invoke(fluidRegistry,type);
                String cell=v[0]+","+v[1]+","+v[2]+" "+id;
                pendingFluids.add(cell);
                protoOrder.computeIfAbsent(chunk.getKey(),key->new ArrayList<>()).add(cell);
            }
            protoOutput.add(test.get("id").getAsString()+"\t"+new Gson().toJson(protoOrder));
            Collections.sort(pendingFluids); // membership only: cross-chunk tick delivery is a separate gate
            output.add(test.get("id").getAsString()+"\t"+result+"\t"+cells.size()+"\t"+digest(String.join("",cells))
                +"\t"+digest(writes.toString())+"\t"+probe+"\t"+new Gson().toJson(heights)+"\t"+new Gson().toJson(inputs)+"\t"+new Gson().toJson(pendingFluids));
            System.out.println("FOSSIL_NATIVE "+test.get("id").getAsString()+" result="+result+" cells="+cells.size());
        }
        Files.write(Path.of(args[1]),output,StandardCharsets.UTF_8);
        if(args.length==3)Files.write(Path.of(args[2]),protoOutput,StandardCharsets.UTF_8);
        System.out.println("FOSSIL_NATIVE_CASES "+output.size());
    }
}
