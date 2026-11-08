// Prints what the source's interpreter (LuaJ, run by the source player)
// gives for each presentation-program operation over a grid of numbers,
// as `op a b = result`, every number as its kind and its IEEE bits, so a
// test can compare the program's numerics with it exactly.
//
// Run with the pinned jar on the class path (see `luaj_table.sh`); the
// output is checked in as `crates/skin-beatoraja/tests/luaj_numerics.txt`.

import org.luaj.vm2.Globals;
import org.luaj.vm2.LuaError;
import org.luaj.vm2.LuaValue;
import org.luaj.vm2.lib.jse.JsePlatform;

public class Probe {
    static final double[] FLOATS = {
        0.0, -0.0, 0.5, -0.5, 1.5, -1.5, 2.0, -7.0, 0.1, 5.5,
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, Double.NaN,
        9007199254740992.0, -9007199254740992.0, 4294967296.0, 2147483648.0, 1e300,
    };
    static final long[] INTS = {
        0, 1, -1, 2, -2, 3, -3, 7, -7, 2147483647L, -2147483648L,
        9007199254740991L, -9007199254740991L,
    };

    static String show(LuaValue v) {
        if (v.isint()) {
            return "i:" + v.toint();
        }
        if (v.isnumber()) {
            return "f:" + Long.toHexString(Double.doubleToRawLongBits(v.todouble()));
        }
        return v.isnil() ? "nil" : (v.toboolean() ? "true" : "false");
    }

    static LuaValue num(String s) {
        if (s.startsWith("i:")) {
            long n = Long.parseLong(s.substring(2));
            return (n == (int) n) ? LuaValue.valueOf((int) n) : LuaValue.valueOf((double) n);
        }
        return LuaValue.valueOf(Double.longBitsToDouble(Long.parseUnsignedLong(s.substring(2), 16)));
    }

    interface Op { LuaValue apply(LuaValue a, LuaValue b); }

    public static void main(String[] args) {
        Globals g = JsePlatform.standardGlobals();
        LuaValue math = g.get("math");
        java.util.List<String> vals = new java.util.ArrayList<>();
        for (long n : INTS) vals.add("i:" + n);
        for (double d : FLOATS) vals.add("f:" + Long.toHexString(Double.doubleToRawLongBits(d)));
        String[] bin = {"add", "sub", "mul", "div", "mod", "lt", "le", "eq", "max", "min"};
        Op[] ops = {
            (a, b) -> a.add(b), (a, b) -> a.sub(b), (a, b) -> a.mul(b), (a, b) -> a.div(b),
            (a, b) -> a.mod(b), (a, b) -> a.lt(b), (a, b) -> a.lteq(b), (a, b) -> a.eq(b),
            (a, b) -> math.get("max").call(a, b), (a, b) -> math.get("min").call(a, b),
        };
        for (int k = 0; k < bin.length; k++) {
            for (String a : vals) {
                for (String b : vals) {
                    String r;
                    try {
                        r = show(ops[k].apply(num(a), num(b)));
                    } catch (LuaError | ArithmeticException e) {
                        r = "error";
                    }
                    System.out.println(bin[k] + " " + a + " " + b + " = " + r);
                }
            }
        }
        // The consumer's conversion of a number's value
        // (`SkinLuaAccessor.java:640-651`), nil and booleans included.
        for (String a : vals) {
            System.out.println("toint " + a + " = i:" + num(a).toint());
        }
        System.out.println("toint nil = i:" + LuaValue.NIL.toint());
        System.out.println("toint true = i:" + LuaValue.TRUE.toint());
        System.out.println("toint false = i:" + LuaValue.FALSE.toint());
        String[] un = {"neg", "floor", "ceil", "abs"};
        for (String a : vals) {
            for (String u : un) {
                String r;
                try {
                    LuaValue x = num(a);
                    r = show(u.equals("neg") ? x.neg() : math.get(u).call(x));
                } catch (LuaError | ArithmeticException e) {
                    r = "error";
                }
                System.out.println(u + " " + a + " = " + r);
            }
        }
    }
}
