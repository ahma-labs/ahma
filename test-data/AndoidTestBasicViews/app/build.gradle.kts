plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
}

// Load Android version from the properties file.
// VERSION_CODE is a monotonic integer for Google Play (never re-use one).
// VERSION_NAME mirrors the Cargo workspace semver and is for display only.
// Run `cargo xtask bump-android-version` before every release build.
val androidVersionProps = java.util.Properties().apply {
    val propsFile = rootProject.file("../android-version.properties")
    if (!propsFile.exists()) {
        throw GradleException(
            "android-version.properties not found at ${propsFile.absolutePath}.\n" +
            "Run: cargo xtask bump-android-version"
        )
    }
    load(propsFile.inputStream())
}
val playVersionCode: Int = androidVersionProps.getProperty("VERSION_CODE")?.toIntOrNull()
    ?: throw GradleException("VERSION_CODE missing or not an integer in android-version.properties")
val playVersionName: String = androidVersionProps.getProperty("VERSION_NAME")
    ?: throw GradleException("VERSION_NAME missing in android-version.properties")

android {
    namespace = "com.example.androidtestbasicviews"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.example.androidtestbasicviews"
        minSdk = 24
        targetSdk = 36
        versionCode = playVersionCode
        versionName = playVersionName

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }
    kotlinOptions {
        jvmTarget = "11"
    }
    buildFeatures {
        viewBinding = true
    }
}

dependencies {

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.appcompat)
    implementation(libs.material)
    implementation(libs.androidx.constraintlayout)
    implementation(libs.androidx.navigation.fragment.ktx)
    implementation(libs.androidx.navigation.ui.ktx)
    testImplementation(libs.junit)
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.espresso.core)
}