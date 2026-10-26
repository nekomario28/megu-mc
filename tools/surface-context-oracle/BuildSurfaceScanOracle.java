// Compare actual SurfaceSystem.buildSurface on a synthetic stone/water column.
// No server/world: a ProtoChunk is filled directly, then the real surface scan runs.
package dev.mcspine.tools;
import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
public final class BuildSurfaceScanOracle {
 static Class<?> cls(String n)throws Exception{return Class.forName(n);}
 static Object field(Class<?> c,String n)throws Exception{Field f=c.getDeclaredField(n);f.setAccessible(true);return f.get(null);}
 static Object get(Object o,String n)throws Exception{Field f=o.getClass().getDeclaredField(n);f.setAccessible(true);return f.get(o);}
 static Object state(String block)throws Exception{Object b=field(cls("dga"),block);return b.getClass().getMethod("o").invoke(b);}
 static Object setupAccess()throws Exception{return null;}
 public static void main(String[] args)throws Exception{
  cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
  Object builtin=field(cls("lt"),"aA");Object access=cls("ka").getMethod("a",cls("jz")).invoke(null,builtin);
  Object pack=cls("ats").getMethod("b").invoke(null);
  Object manager=cls("atx").getConstructor(cls("ass"),List.class).newInstance(field(cls("ass"),"b"),List.of(pack));
  access=cls("akm").getMethod("a",cls("aue"),cls("ka"),List.class).invoke(null,manager,access,field(cls("akm"),"a"));
  Object settingsKey=field(cls("dzd"),"c");Object settingsRegKey=settingsKey.getClass().getMethod("c").invoke(settingsKey);
  Object settingsReg=access.getClass().getMethod("d",cls("akq")).invoke(access,settingsRegKey);
  Object settings=settingsReg.getClass().getMethod("f",cls("akq")).invoke(settingsReg,settingsKey);
  Object settingsHolder=((Optional<?>)settingsReg.getClass().getMethod("b",cls("akq")).invoke(settingsReg,settingsKey)).orElseThrow();
  Object noiseReg=access.getClass().getMethod("d",cls("akq")).invoke(access,field(cls("lu"),"aP"));
  Object noiseGetter=noiseReg.getClass().getMethod("q").invoke(noiseReg);
  Object randomState=cls("dzm").getMethod("a",cls("dzd"),cls("jn"),long.class).invoke(null,settings,noiseGetter,0L);
  Object biomeReg=access.getClass().getMethod("d",cls("akq")).invoke(access,field(cls("lu"),"aF"));
  Object plains=((Optional<?>)biomeReg.getClass().getMethod("c",cls("akr")).invoke(biomeReg,cls("akr").getMethod("b",String.class).invoke(null,"plains"))).orElseThrow();
  Object fixed=cls("deh").getConstructor(cls("jm")).newInstance(plains);
  Object bm=cls("ddy").getConstructor(cls("ddy$a"),long.class).newInstance(fixed,0L);
  Object height=cls("dcy").getMethod("e",int.class,int.class).invoke(null,-64,384);
  Object generator=cls("dzb").getConstructor(cls("dea"),cls("jm")).newInstance(fixed,settingsHolder);
  Object wg=cls("dzv").getConstructor(cls("duz"),cls("dcy")).newInstance(generator,height);
  Object system=randomState.getClass().getMethod("c").invoke(randomState);
  Object stone=state("b"), water=state("G"), gold=state("ch");
  Class<?> blockState=cls("dtc"), posC=cls("jd"), heightType=cls("dyy$a"), cave=cls("eil");
  Constructor<?> pos=posC.getConstructor(int.class,int.class,int.class);
  Method set=cls("dvs").getMethod("a",posC,blockState,boolean.class);
  Method read=cls("dvs").getMethod("a_",posC);
  Method heightmap=cls("duy").getMethod("a",heightType,int.class,int.class);
  Object ocean=field(heightType,"d");
  Method fill=cls("dvs").getConstructor(cls("dcd"),cls("dvv"),cls("dcy"),cls("jz"),cls("eab")).getDeclaringClass().getMethod("a",posC,blockState,boolean.class);
  Method build=system.getClass().getMethod("a",cls("dzm"),cls("ddy"),cls("jz"),boolean.class,cls("dzv"),cls("duy"),cls("dzc"),cls("dzp$o"));
  JsonArray out=new JsonArray();
  for(int offset=0;offset<=2;offset++){
   Object chunk=cls("dvs").getConstructor(cls("dcd"),cls("dvv"),cls("dcy"),cls("jz"),cls("eab")).newInstance(cls("dcd").getConstructor(int.class,int.class).newInstance(0,0),field(cls("dvv"),"a"),height,biomeReg,null);
   for(int x=0;x<16;x++)for(int z=0;z<16;z++){
    int top=50+((x+z)%11); int sea=63;
    for(int y=-64;y<=top;y++)set.invoke(chunk,pos.newInstance(x,y,z),stone,false);
    for(int y=top+1;y<=sea;y++)set.invoke(chunk,pos.newInstance(x,y,z),water,false);
   }
   Object cond=cls("dzp").getMethod("a",int.class,boolean.class,int.class,cave).invoke(null,offset,false,0,field(cave,"b"));
   Object rule=cls("dzp").getMethod("a",cls("dzp$f"),cls("dzp$o")).invoke(null,cond,cls("dzp").getMethod("a",blockState).invoke(null,gold));
   build.invoke(system,randomState,bm,biomeReg,false,wg,chunk,null,rule);
   for(int x=0;x<16;x++)for(int z=0;z<16;z++){
    int top=50+((x+z)%11); JsonArray golds=new JsonArray();
    for(int y=top-5;y<=top+1;y++){
     if(read.invoke(chunk,pos.newInstance(x,y,z)).equals(gold))golds.add(y);
    }
    JsonObject row=new JsonObject();row.addProperty("offset",offset);row.addProperty("x",x);row.addProperty("z",z);
    row.addProperty("top",top);row.addProperty("sea",63);row.add("gold",golds);out.add(row);
   }
  }
  JsonObject root=new JsonObject();root.addProperty("rows",out.size());root.add("cases",out);
  Files.writeString(Path.of(args[0]),root.toString());
  System.out.println("buildSurface scan oracle: "+out.size()+" columns");
 }
}
