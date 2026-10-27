package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Real TreeFeature.updateLeaves, before shape-edge updates or scheduled ticks. */
public class LeafDistanceOracle {
    record P(int x,int y,int z) {
        P plus(P p) { return new P(x+p.x,y+p.y,z+p.z); }
        public String toString() { return x+","+y+","+z; }
    }
    record Scene(P min,P max,List<P> logs,List<P> blocked,Map<P,String> cells) {}
    static Class<?> cls(String n) throws Exception { return Class.forName(n); }
    static Method x,y,z,get,set,optional;
    static Object registry,distance,persistent,waterlogged,air;
    static final Map<String,Object> states=new HashMap<>();
    static final P[] DIRS={new P(0,-1,0),new P(0,1,0),new P(0,0,-1),new P(0,0,1),new P(-1,0,0),new P(1,0,0)};
    static P point(Object p) throws Exception { return new P((int)x.invoke(p),(int)y.invoke(p),(int)z.invoke(p)); }
    static Object state(String key) throws Exception {
        if(states.containsKey(key)) return states.get(key);
        String name=key.equals("T")?"oak_log":key.equals("S")?"stone":key.equals("A")?"air":key.startsWith("C")?"scaffolding":"oak_leaves";
        Object id=cls("akr").getMethod("a",String.class).invoke(null,"minecraft:"+name);
        Object block=cls("jz").getMethod("a",cls("akr")).invoke(registry,id);
        Object s=cls("dfy").getMethod("o").invoke(block);
        if(key.startsWith("C")) s=set.invoke(s,cls("dmo").getField("c").get(null),Integer.parseInt(key.substring(1)));
        else if(key.length()==2) {
            s=set.invoke(s,distance,Integer.parseInt(key.substring(1)));
            s=set.invoke(s,persistent,key.charAt(0)=='P'||key.charAt(0)=='B');
            s=set.invoke(s,waterlogged,key.charAt(0)=='W'||key.charAt(0)=='B');
        }
        states.put(key,s);return s;
    }
    static String leaf(Object s) throws Exception {
        int d=(int)get.invoke(s,distance);
        boolean p=(boolean)get.invoke(s,persistent),w=(boolean)get.invoke(s,waterlogged);
        return (p?(w?"B":"P"):(w?"W":"L"))+d;
    }
    static Scene scene(int k) {
        List<P> logs=new ArrayList<>(),blocked=new ArrayList<>();Map<P,String> cells=new HashMap<>();
        P min=new P(0,0,0),max=new P(6,0,0);logs.add(min);cells.put(min,"T");
        for(int i=1;i<=6;i++) cells.put(new P(i,0,0),"L7");
        if(k==1) { cells.put(new P(2,0,0),"C7");cells.put(new P(5,0,0),"T"); }
        if(k==2) { max=new P(6,1,1);cells.put(new P(1,1,0),"P1");cells.put(new P(2,1,0),"W7");cells.put(new P(2,0,1),"B7"); }
        if(k==3) blocked.add(new P(2,0,0));
        if(k==4) { cells.remove(new P(1,0,0));cells.put(new P(3,0,0),"L1"); }
        if(k==5) cells.put(new P(6,0,0),"T");
        if(k==6) { logs.add(new P(6,0,0));cells.put(new P(6,0,0),"T"); }
        if(k>=7) {
            cells.clear();logs.clear();max=new P(6,4,6);Random r=new Random(k-7);
            logs.add(min);logs.add(max);
            for(int px=0;px<=6;px++) for(int py=0;py<=4;py++) for(int pz=0;pz<=6;pz++) {
                P p=new P(px,py,pz);int v=r.nextInt(10);
                if(v==0) continue;
                String s=v==1?"T":v==2?"S":"";
                if(v>2) s=""+"LPWB".charAt(r.nextInt(4))+(1+r.nextInt(7));
                cells.put(p,s);
                if(v==3) blocked.add(p);
            }
            cells.put(min,"T");cells.put(max,"T");
        }
        return new Scene(min,max,logs,blocked,cells);
    }
    static boolean inside(P p,P min,P max) { return p.x>=min.x&&p.y>=min.y&&p.z>=min.z&&p.x<=max.x&&p.y<=max.y&&p.z<=max.z; }
    // Experiment only: change HashSet to FIFO while retaining native cursor semantics.
    static void fifo(Map<P,Object> world,Scene scene,P origin,Set<Object> nativeLogs) throws Exception {
        List<Set<P>> q=new ArrayList<>();for(int d=0;d<7;d++) q.add(new LinkedHashSet<>());
        Set<P> visited=new HashSet<>();for(P p:scene.blocked) visited.add(p.plus(origin));
        for(Object p:nativeLogs) q.get(0).add(point(p));
        P min=scene.min.plus(origin),max=scene.max.plus(origin);int depth=0;
        while(depth<7) {
            if(q.get(depth).isEmpty()) {depth++;continue;}
            Iterator<P> it=q.get(depth).iterator();P p=it.next();it.remove();if(!inside(p,min,max)) continue;
            if(depth!=0) world.put(p,set.invoke(world.getOrDefault(p,air),distance,depth));
            visited.add(p);
            for(P dir:DIRS) {
                P n=p.plus(dir);if(!inside(n,min,max)||visited.contains(n)) continue;
                OptionalInt d=(OptionalInt)optional.invoke(null,world.getOrDefault(n,air));
                if(d.isEmpty()) continue;
                int next=Math.min(d.getAsInt(),depth+1);
                if(next<7) {q.get(next).add(n);depth=Math.min(depth,next);}
            }
        }
    }
    static String points(Collection<P> points) { return points.stream().map(P::toString).collect(java.util.stream.Collectors.joining(";")); }
    static String cells(Map<P,String> cells) {
        return cells.entrySet().stream().map(e->e.getKey()+":"+e.getValue()).sorted().collect(java.util.stream.Collectors.joining(";"));
    }
    static Map<P,String> leaves(Map<P,Object> world,P origin) throws Exception {
        Map<P,String> result=new HashMap<>();
        for(var e:world.entrySet()) if((boolean)cls("dtc").getMethod("b",cls("duf")).invoke(e.getValue(),distance)) {
            P p=e.getKey();result.put(new P(p.x-origin.x,p.y-origin.y,p.z-origin.z),leaf(e.getValue()));
        }
        return result;
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        registry=cls("lt").getField("e").get(null);VanillaBlockTags.load(registry);
        Class<?> pos=cls("jd"),bs=cls("dtc"),level=cls("dcx");
        x=pos.getMethod("u");y=pos.getMethod("v");z=pos.getMethod("w");get=bs.getMethod("c",cls("duf"));
        set=bs.getMethod("a",cls("duf"),Comparable.class);optional=cls("dki").getMethod("n",bs);
        distance=cls("dki").getField("d").get(null);persistent=cls("dki").getField("e").get(null);waterlogged=cls("dki").getField("f").get(null);
        air=state("A");if(!optional.invoke(null,state("T")).equals(OptionalInt.of(0))) throw new AssertionError("unbound logs");
        if(!optional.invoke(null,state("C7")).equals(OptionalInt.empty())) throw new AssertionError("scaffolding is not the leaf distance property");
        Constructor<?> position=pos.getConstructor(int.class,int.class,int.class);
        Constructor<?> bounds=cls("ejj").getConstructor(int.class,int.class,int.class,int.class,int.class,int.class);
        Method update=cls("edq").getDeclaredMethod("a",level,cls("ejj"),Set.class,Set.class,Set.class);update.setAccessible(true);
        List<String> rows=new ArrayList<>();int fifoDifferences=0;
        for(int kind=0;kind<39;kind++) for(P origin:List.of(new P(0,16,0),new P(-1024,70,512),new P(60007,16,-60007))) {
            Scene scene=scene(kind);Map<P,Object> world=new HashMap<>();Set<P> writes=new HashSet<>();
            for(var e:scene.cells.entrySet()) world.put(e.getKey().plus(origin),state(e.getValue()));
            Map<P,Object> model=new HashMap<>(world);
            Object proxy=Proxy.newProxyInstance(LeafDistanceOracle.class.getClassLoader(),new Class<?>[]{level},(p,m,a)->{
                if(m.getName().equals("a_")&&a.length==1) return world.getOrDefault(point(a[0]),air);
                if(m.getName().equals("a")&&a.length==3&&pos.isInstance(a[0])&&bs.isInstance(a[1])) {
                    if((int)a[2]!=19) throw new AssertionError("unexpected write flags: "+a[2]);
                    P q=point(a[0]);world.put(q,a[1]);writes.add(q);return true;
                }
                throw new UnsupportedOperationException(m.toString());
            });
            Set<Object> logs=new HashSet<>(),blocked=new HashSet<>();
            for(P p:scene.logs) {p=p.plus(origin);logs.add(position.newInstance(p.x,p.y,p.z));}
            for(P p:scene.blocked) {p=p.plus(origin);blocked.add(position.newInstance(p.x,p.y,p.z));}
            P min=scene.min.plus(origin),max=scene.max.plus(origin);
            update.invoke(null,proxy,bounds.newInstance(min.x,min.y,min.z,max.x,max.y,max.z),logs,blocked,Set.of());
            fifo(model,scene,origin,logs);
            String actual=cells(leaves(world,origin));boolean same=actual.equals(cells(leaves(model,origin)));if(!same) fifoDifferences++;
            rows.add(kind+"\t"+origin+"\t"+scene.min+"\t"+scene.max+"\t"+points(scene.logs)+"\t"+points(scene.blocked)+"\t"+cells(scene.cells)+"\t"+actual+"\t"+writes.size()+"\t"+same);
        }
        Files.write(Path.of(args[0]),rows);
        LeafQueueOracle.write(Path.of(args[1]));
        System.out.println("leaf oracle: "+rows.size()+" actual updateLeaves vectors; FIFO differs in "+fifoDifferences);
    }
}
