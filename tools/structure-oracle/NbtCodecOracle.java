package dev.mcspine.tools;
import java.io.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual NbtIo and Java modified UTF-8 behavior, including malformed inputs. */
public final class NbtCodecOracle {
    interface Writer { void write(DataOutputStream out) throws Exception; }
    static byte[] document(Writer writer) throws Exception {
        ByteArrayOutputStream buffer=new ByteArrayOutputStream();DataOutputStream out=new DataOutputStream(buffer);
        out.writeByte(10);out.writeUTF("");writer.write(out);out.writeByte(0);return buffer.toByteArray();
    }
    static void header(DataOutputStream out,int type,String key) throws IOException { out.writeByte(type);out.writeUTF(key); }
    public static void main(String[] args) throws Exception {
        StructureTemplateOracle.initNbt();
        List<byte[]> inputs=new ArrayList<>();
        inputs.add(document(out->{
            header(out,1,"byte");out.writeByte(-128);header(out,2,"short");out.writeShort(-32768);
            header(out,3,"int");out.writeInt(Integer.MIN_VALUE);header(out,4,"long");out.writeLong(Long.MIN_VALUE);
            header(out,5,"float");out.writeInt(0x7fa12345);header(out,6,"double");out.writeLong(0x7ff123456789abcdL);
            header(out,7,"bytes");out.writeInt(3);out.write(new byte[]{0,-1,127});
            header(out,8,"string");out.writeUTF("\u0000\ud83d\ude00\ud800X\udfff");
            header(out,9,"list");out.writeByte(3);out.writeInt(2);out.writeInt(-1);out.writeInt(2);
            header(out,10,"compound");header(out,8,"nested");out.writeUTF("value");out.writeByte(0);
            header(out,11,"ints");out.writeInt(2);out.writeInt(Integer.MIN_VALUE);out.writeInt(Integer.MAX_VALUE);
            header(out,12,"longs");out.writeInt(2);out.writeLong(Long.MIN_VALUE);out.writeLong(Long.MAX_VALUE);
        }));
        for(int kind=0;kind<=12;kind++) { int type=kind;
            inputs.add(document(out->{header(out,9,"empty");out.writeByte(type);out.writeInt(0);}));
        }
        inputs.add(document(out->{header(out,3,"duplicate");out.writeInt(1);header(out,8,"duplicate");out.writeUTF("last");}));
        for(byte[] bytes:new byte[][]{{0},{(byte)0xc0,(byte)0x80},{(byte)0xc1,(byte)0x81},{(byte)0xe0,(byte)0x80,(byte)0x80},{(byte)0xed,(byte)0xa0,(byte)0x80},{(byte)0xf0,(byte)0x9f,(byte)0x98,(byte)0x80},{(byte)0xc0},{(byte)0xe0,(byte)0x80},{(byte)0xc1,0},{(byte)0x80}}) {
            inputs.add(document(out->{header(out,8,"raw");out.writeShort(bytes.length);out.write(bytes);}));
        }
        for(int kind:new int[]{7,9,11,12}) { inputs.add(document(out->{header(out,kind,"negative");if(kind==9)out.writeByte(1);out.writeInt(-1);})); }
        for(int depth:new int[]{1,511,512,513}) {
            inputs.add(document(out->{for(int i=1;i<depth;i++)header(out,10,"child");for(int i=1;i<depth;i++)out.writeByte(0);}));
        }
        byte[] valid=inputs.getFirst();
        for(int cut:new int[]{0,1,2,3,5,8,16,valid.length-1}) inputs.add(Arrays.copyOf(valid,cut));
        Method read=Class.forName("uo").getMethod("a",DataInput.class,Class.forName("uk"));
        Method unlimited=Class.forName("uk").getMethod("a");List<String> rows=new ArrayList<>();
        for(byte[] bytes:inputs) {
            String result;
            try {
                Object value=read.invoke(null,new DataInputStream(new ByteArrayInputStream(bytes)),unlimited.invoke(null));
                ByteArrayOutputStream buffer=new ByteArrayOutputStream();StructureTemplateOracle.named(value,new DataOutputStream(buffer));
                result=HexFormat.of().formatHex(buffer.toByteArray());
            } catch(InvocationTargetException error) { result="REJECT"; }
            rows.add(HexFormat.of().formatHex(bytes)+"\t"+result);
        }
        Files.write(Path.of(args[0]),rows);System.out.println("native NBT codec cases: "+rows.size());
    }
}
