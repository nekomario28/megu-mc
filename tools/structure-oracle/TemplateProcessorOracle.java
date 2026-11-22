package dev.megu.tools;

import com.google.gson.*;
import com.mojang.serialization.Codec;
import java.io.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicInteger;

/** Actual StructureTemplate.processBlockInfos, not a reimplementation of its loop. */
public final class TemplateProcessorOracle {
    static Object pos(JsonArray a) throws Exception {
        return NativeHarness.pos(a.get(0).getAsInt(), a.get(1).getAsInt(), a.get(2).getAsInt());
    }
    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        StructureTemplateOracle.initNbt();
        Class<?> settings=Class.forName("enp"), info=Class.forName("ent$c"),
                position=Class.forName("jd"), rngClass=Class.forName("ayw"), processor=Class.forName("enq");
        Codec<?> codec=(Codec<?>)Class.forName("ens").getField("a").get(null);
        Method process=Class.forName("ent").getMethod("a",Class.forName("ddl"),position,position,settings,List.class);
        Constructor<?> blockInfo=info.getConstructor(position,Class.forName("dtc"),Class.forName("ub"));
        // Preserve native codec surprises as explicit rejection controls, not skipped cases.
        String[][] invalid={
            {"{\"processor_type\":\"minecraft:protected_blocks\",\"value\":\"bedrock\"}","Not a tag id"},
            {"{\"processor_type\":\"minecraft:block_rot\",\"integrity\":-0.0}","outside of range"},
            {"{\"processor_type\":\"minecraft:block_rot\",\"integrity\":-1e-60}","outside of range"}
        };
        for(String[] control:invalid) {
            boolean rejected=false;
            try { NativeHarness.getOrThrow(NativeHarness.parse(codec,JsonParser.parseString(control[0]))); }
            catch(InvocationTargetException ex) {
                if(!(ex.getCause() instanceof IllegalStateException) || !ex.getCause().getMessage().contains(control[1])) throw ex;
                rejected=true;
            }
            if(!rejected) throw new AssertionError("native accepted invalid processor "+control[0]);
        }
        List<String> output=new ArrayList<>();
        JsonArray cases=JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonArray();
        for(JsonElement element:cases) {
            JsonObject c=element.getAsJsonObject();
            Object opts=settings.getConstructor().newInstance();
            for(String[] pair:new String[][]{{"dkv","mirror"},{"dmm","rotation"}}) {
                Class<?> type=Class.forName(pair[0]);
                Object choice=type.getEnumConstants()[c.get(pair[1]).getAsInt()];
                settings.getMethod("a",type).invoke(opts,choice);
            }
            settings.getMethod("a",position).invoke(opts,pos(c.getAsJsonArray("pivot")));
            // processBlockInfos deliberately does not clip; placeInWorld clips later.
            if(c.get("clip_out").getAsBoolean()) settings.getMethod("a",Class.forName("ejj"))
                    .invoke(opts,Class.forName("ejj").getConstructor(int.class,int.class,int.class,int.class,int.class,int.class)
                            .newInstance(-500,-500,-500,-400,-400,-400));
            AtomicInteger sharedDraws=new AtomicInteger(), reads=new AtomicInteger();
            Object rng=NativeHarness.rng(c.get("seed").getAsLong());
            String mode=c.get("random").getAsString();
            if(!mode.equals("positional")) {
                if(!mode.equals("shared") && !mode.equals("zero")) throw new AssertionError("random mode "+mode);
                Object observed=Proxy.newProxyInstance(rngClass.getClassLoader(),new Class<?>[]{rngClass},(proxy,method,a)->{
                    if(method.getName().equals("i") && method.getParameterCount()==0) {
                        sharedDraws.incrementAndGet();
                        if(mode.equals("zero")) return 0.0f;
                    }
                    try { return method.invoke(rng,a); }
                    catch(InvocationTargetException ex) { throw ex.getCause(); }
                });
                settings.getMethod("a",rngClass).invoke(opts,observed);
            }
            for(JsonElement p:c.getAsJsonArray("processors")) {
                try {
                    Object decoded=NativeHarness.getOrThrow(NativeHarness.parse(codec,p));
                    settings.getMethod("a",processor).invoke(opts,decoded);
                } catch(Exception ex) { throw new IllegalStateException("case="+c.get("id")+" processor="+p,ex); }
            }
            List<Object> blocks=new ArrayList<>();
            if(c.has("template")) {
                Class<?> template=Class.forName("ent"), compound=Class.forName("ub"), budget=Class.forName("uk");
                Object root=Class.forName("uo").getMethod("a",Path.class,budget).invoke(null,
                        Path.of(args[2]).resolve(c.get("template").getAsString()),budget.getMethod("a").invoke(null));
                Object registry=Class.forName("lt").getField("e").get(null);
                Object lookup=Class.forName("jz").getMethod("q").invoke(registry);
                Object loaded=template.getConstructor().newInstance();
                template.getMethod("a",Class.forName("jn"),compound).invoke(loaded,lookup,root);
                Field palettes=template.getDeclaredField("l");palettes.setAccessible(true);
                Object palette=((List<?>)palettes.get(loaded)).get(0);
                blocks.addAll((List<?>)Class.forName("ent$a").getMethod("a").invoke(palette));
            }
            for(JsonElement b:c.has("blocks")?c.getAsJsonArray("blocks"):new JsonArray()) {
                JsonObject block=b.getAsJsonObject();
                Object nbt=null;
                if(block.has("marker")) {
                    nbt=Class.forName("ub").getConstructor().newInstance();
                    Class.forName("ub").getMethod("a",String.class,int.class).invoke(nbt,"marker",block.get("marker").getAsInt());
                }
                blocks.add(blockInfo.newInstance(pos(block.getAsJsonArray("pos")),NativeHarness.stateFromJson(block.get("state")),nbt));
            }
            List<Object> world=new ArrayList<>();
            for(JsonElement state:c.getAsJsonArray("world")) world.add(NativeHarness.stateFromJson(state));
            var level=NativeHarness.levelFromStates((x,y,z)->{
                reads.incrementAndGet();
                return world.get(Math.floorMod(x*31+y*7+z*13,world.size()));
            });
            Object origin=pos(c.getAsJsonArray("origin"));
            List<?> processed=(List<?>)process.invoke(null,level.proxy,origin,origin,opts,blocks);
            ByteArrayOutputStream bytes=new ByteArrayOutputStream();
            DataOutputStream out=new DataOutputStream(bytes);
            out.writeInt(1); out.writeInt(processed.size());
            for(Object b:processed) StructureTemplateOracle.writeBlockInfo(b,out);
            int probe=(int)rngClass.getMethod("a",int.class).invoke(rng,1000003);
            output.add(c.get("id").getAsString()+"\t"+processed.size()+"\t"+StructureTemplateOracle.hash(bytes.toByteArray())
                    +"\t"+probe+"\t"+sharedDraws.get()+"\t"+reads.get());
        }
        if(output.size()!=cases.size() || output.isEmpty()) throw new AssertionError("missing processor cases");
        Files.write(Path.of(args[1]),output);
        System.out.println("native processor cases: "+output.size());
    }
}
