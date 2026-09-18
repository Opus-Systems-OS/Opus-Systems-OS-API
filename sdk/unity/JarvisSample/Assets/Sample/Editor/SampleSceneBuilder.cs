using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;

/// <summary>
/// Builds Assets/Sample/JarvisSample.unity: a camera and one GameObject with
/// JarvisConsole. Run from the CLI:
///   Unity -batchmode -projectPath . -executeMethod SampleSceneBuilder.Build -quit
/// so the committed scene never depends on anyone's editor session.
/// </summary>
public static class SampleSceneBuilder
{
    public static void Build()
    {
        var scene = EditorSceneManager.NewScene(NewSceneSetup.DefaultGameObjects, NewSceneMode.Single);
        var go = new GameObject("Jarvis");
        go.AddComponent<JarvisConsole>();
        EditorSceneManager.SaveScene(scene, "Assets/Sample/JarvisSample.unity");
        EditorBuildSettings.scenes = new[] { new EditorBuildSettingsScene("Assets/Sample/JarvisSample.unity", true) };
        AssetDatabase.SaveAssets();
        Debug.Log("SampleSceneBuilder: wrote Assets/Sample/JarvisSample.unity");
    }
}
