# Strip verbose/debug logging in release builds while preserving info/warn/error logs.
-assumenosideeffects class android.util.Log {
    public static boolean isLoggable(java.lang.String, int);
    public static int v(...);
    public static int d(...);
}

# Keep source file names and line numbers for readable crash stacktraces.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile
