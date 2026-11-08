# Classes appelées depuis le code natif (JNI) : ne jamais renommer/supprimer.
-keep class com.nonpolynomial.** { *; }
-keep class io.github.gedgygedgy.** { *; }
-keep class com.lesjeuxducycle.** { *; }
-keepclasseswithmembernames class * { native <methods>; }
