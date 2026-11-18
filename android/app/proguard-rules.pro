# Strip verbose/debug logging in release builds while preserving info/warn/error logs.
-assumenosideeffects class android.util.Log {
    public static boolean isLoggable(java.lang.String, int);
    public static int v(...);
    public static int d(...);
}

# Keep source file names and line numbers for readable crash stacktraces.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile

# Keep C/C++ JNI bridge objects and external methods intact under R8 full mode.
-keep class io.jeph.oopbot.syntax.TreeSitter { *; }
-keep class io.jeph.oopbot.localmodel.engine.OpenClProbe { *; }
-keep class io.jeph.oopbot.localmodel.engine.llama.LlamaServer { *; }
