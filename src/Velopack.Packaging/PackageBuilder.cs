﻿using System;
using System.Collections.Generic;
using System.Linq;
using System.Reflection.Metadata.Ecma335;
using System.Security;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading.Channels;
using System.Threading.Tasks;
using System.Xml;
using Microsoft.Extensions.Logging;
using NuGet.Versioning;
using Spectre.Console;
using Velopack.Compression;
using Velopack.NuGet;

namespace Velopack.Packaging
{
    public interface IPackOptions : INugetPackCommand
    {
        RID TargetRuntime { get; }
        DirectoryInfo ReleaseDir { get; }
        string Channel { get; }
        DeltaMode DeltaMode { get; }
    }

    public abstract class PackageBuilder<T> : ICommand<T>
        where T : class, IPackOptions
    {
        protected RuntimeOs SupportedTargetOs { get; }

        protected ILogger Log { get; }

        protected DirectoryInfo TempDir { get; private set; }

        protected T Options { get; private set; }

        private readonly Regex REGEX_EXCLUDES = new Regex(@".*[\\\/]createdump.*|.*\.vshost\..*|.*\.nupkg$|.*\.pdb$", RegexOptions.IgnoreCase | RegexOptions.Compiled);

        public PackageBuilder(RuntimeOs supportedOs, ILogger logger)
        {
            SupportedTargetOs = supportedOs;
            Log = logger;
        }

        public async Task Run(T options)
        {
            if (options.TargetRuntime?.BaseRID != SupportedTargetOs)
                throw new ArgumentException($"Target runtime must be {SupportedTargetOs}.", nameof(options.TargetRuntime));

            Log.Info("Beginning to package release.");
            Log.Info("Releases Directory: " + options.ReleaseDir.FullName);

            var releaseDir = options.ReleaseDir;
            var channel = options.Channel?.ToLower() ?? ReleaseEntryHelper.GetDefaultChannel(SupportedTargetOs);

            var entryHelper = new ReleaseEntryHelper(releaseDir.FullName, Log);
            entryHelper.ValidateChannelForPackaging(SemanticVersion.Parse(options.PackVersion), channel, options.TargetRuntime);

            var packId = options.PackId;
            var packTitle = options.PackTitle ?? options.PackId;
            var packAuthors = options.PackAuthors ?? options.PackId;
            var packDirectory = options.PackDirectory;
            var packVersion = options.PackVersion;

            var suffix = ReleaseEntryHelper.GetPkgSuffix(SupportedTargetOs, channel);
            if (!String.IsNullOrWhiteSpace(suffix)) {
                packVersion += suffix;
            }

            using var _1 = Utility.GetTempDirectory(out var pkgTempDir);
            TempDir = new DirectoryInfo(pkgTempDir);
            Options = options;

            List<(string from, string to)> filesToCopy = new();

            try {
                await AnsiConsole.Progress()
                    .AutoRefresh(true)
                    .AutoClear(false)
                    .HideCompleted(false)
                    .Columns(new ProgressColumn[]
                    {
                    new SpinnerColumn(),
                    new TaskDescriptionColumn(),
                    new ProgressBarColumn(),
                    new PercentageColumn(),
                    new ElapsedTimeColumn(),
                    })
                    .StartAsync(async ctx => {
                        var taskPreProcess = ctx.AddTask($"[italic]Pre-process steps[/]");
                        taskPreProcess.StartTask();
                        var prev = entryHelper.GetPreviousFullRelease(NuGetVersion.Parse(packVersion), channel);
                        var nuspecText = GenerateNuspecContent(
                            packId, packTitle, packAuthors, packVersion, options.ReleaseNotes, packDirectory);
                        packDirectory = await PreprocessPackDir((p) => taskPreProcess.Value = p, packDirectory, nuspecText);
                        taskPreProcess.StopTask();
                        Log.Info("[bold]Complete: Pre-process steps[/]");

                        if (VelopackRuntimeInfo.IsWindows || VelopackRuntimeInfo.IsOSX) {
                            var taskSigning = ctx.AddTask($"[italic]Code-sign application[/]");
                            taskSigning.StartTask();
                            await CodeSign((p) => taskSigning.Value = p, packDirectory);
                            taskSigning.StopTask();
                            Log.Info("[bold]Complete: Code-sign application[/]");
                        }

                        var portableTask = Task.Run(async () => {
                            var taskPortable = ctx.AddTask($"[italic]Building portable package[/]");
                            taskPortable.StartTask();
                            var suggestedPortable = entryHelper.GetSuggestedPortablePath(packId, channel, options.TargetRuntime);
                            var incomplete = suggestedPortable + ".incomplete";
                            if (File.Exists(incomplete)) File.Delete(incomplete);
                            filesToCopy.Add((incomplete, suggestedPortable));
                            await CreatePortablePackage((p) => taskPortable.Value = p, packDirectory, incomplete);
                            taskPortable.StopTask();
                            Log.Info("[bold]Complete: Build portable package[/]");
                        });

                        var taskNuget = ctx.AddTask($"[italic]Building release {packVersion}[/]");
                        taskNuget.StartTask();
                        var releaseName = new ReleaseEntryName(packId, SemanticVersion.Parse(packVersion), false, Options.TargetRuntime);
                        var releasePath = Path.Combine(releaseDir.FullName, releaseName.ToFileName());
                        if (File.Exists(releasePath)) File.Delete(releasePath);
                        await CreateReleasePackage((p) => taskNuget.Value = p, packDirectory, nuspecText, releasePath);
                        entryHelper.AddNewRelease(releasePath, channel);
                        taskNuget.StopTask();
                        Log.Info("[bold]Complete: Build release package[/]");

                        var setupTask = Task.Run(async () => {
                            var taskSetup = ctx.AddTask($"[italic]Create setup package[/]");
                            taskSetup.StartTask();
                            var suggestedSetup = entryHelper.GetSuggestedSetupPath(packId, channel, options.TargetRuntime);
                            var incomplete = suggestedSetup + ".incomplete";
                            if (File.Exists(incomplete)) File.Delete(incomplete);
                            filesToCopy.Add((incomplete, suggestedSetup));
                            await CreateSetupPackage((p) => taskSetup.Value = p, releasePath, incomplete);
                            taskSetup.StopTask();
                            Log.Info("[bold]Complete: Create setup package[/]");
                        });

                        if (prev != null && options.DeltaMode != DeltaMode.None) {
                            var taskDelta = ctx.AddTask($"[italic]Building delta {prev.Version} -> {packVersion}[/]");
                            taskDelta.StartTask();
                            var deltaPkg = await CreateDeltaPackage((p) => taskDelta.Value = p, releasePath, prev.PackageFile, options.DeltaMode);
                            taskDelta.StopTask();
                            entryHelper.AddNewRelease(deltaPkg, channel);
                            Log.Info("[bold]Complete: Building delta package[/]");
                        }

                        await portableTask;
                        await setupTask;

                        var taskFinish = ctx.AddTask($"[italic]Finishing up[/]");
                        taskFinish.IsIndeterminate = true;
                        taskFinish.StartTask();
                        entryHelper.SaveReleasesFiles();
                        foreach (var f in filesToCopy) {
                            File.Move(f.from, f.to, true);
                        }
                        taskFinish.Value = 100;
                        taskFinish.StopTask();
                    });
                Log.Info("[bold]Done.[/]");
            } catch {
                try {
                    foreach (var f in filesToCopy) {
                        File.Delete(f.from);
                    }
                    entryHelper.RollbackNewReleases();
                } catch (Exception ex) {
                    Log.Error("Failed to remove incomplete releases: " + ex.Message);
                }
                throw;
            }
        }

        protected virtual string GenerateNuspecContent(string packId, string packTitle, string packAuthors, string packVersion, string releaseNotes, string packDir)
        {
            var releaseNotesText = String.IsNullOrEmpty(releaseNotes)
                       ? "" // no releaseNotes
                       : $"<releaseNotes>{SecurityElement.Escape(File.ReadAllText(releaseNotes))}</releaseNotes>";

            string nuspec = $@"
<?xml version=""1.0"" encoding=""utf-8""?>
<package xmlns=""http://schemas.microsoft.com/packaging/2010/07/nuspec.xsd"">
  <metadata>
    <id>{packId}</id>
    <title>{packTitle ?? packId}</title>
    <description>{packTitle ?? packId}</description>
    <authors>{packAuthors ?? packId}</authors>
    <version>{packVersion}</version>
    {releaseNotesText}
  </metadata>
</package>
".Trim();

            return nuspec;
        }

        protected virtual Task<string> PreprocessPackDir(Action<int> progress, string packDir, string nuspecText)
        {
            var dir = TempDir.CreateSubdirectory("PreprocessPackDir");
            CopyFiles(new DirectoryInfo(packDir), dir, progress, true);
            File.WriteAllText(Path.Combine(dir.FullName, "sq.version"), nuspecText);
            return Task.FromResult(packDir);
        }

        protected virtual Task CodeSign(Action<int> progress, string packDir)
        {
            throw new NotImplementedException();
        }

        protected virtual Task CreatePortablePackage(Action<int> progress, string packDir, string outputPath)
        {
            return Task.CompletedTask;
        }

        protected virtual Task<string> CreateDeltaPackage(Action<int> progress, string releasePkg, string prevReleasePkg, DeltaMode mode)
        {
            var deltaBuilder = new DeltaPackageBuilder(Log);
            var deltaOutputPath = releasePkg.Replace("-full", "-delta");
            var (dp, stats) = deltaBuilder.CreateDeltaPackage(new ReleasePackage(prevReleasePkg), new ReleasePackage(releasePkg), deltaOutputPath, mode, progress);
            return Task.FromResult(dp.PackageFile);
        }

        protected virtual Task CreateSetupPackage(Action<int> progress, string releasePkg, string outputPath)
        {
            return Task.CompletedTask;
        }

        protected virtual async Task CreateReleasePackage(Action<int> progress, string packDir, string nuspecText, string outputPath)
        {
            var stagingDir = TempDir.CreateSubdirectory("CreateReleasePackage");

            var nuspecPath = Path.Combine(stagingDir.FullName, Options.PackId + ".nuspec");
            File.WriteAllText(nuspecPath, nuspecText);

            var appDir = stagingDir.CreateSubdirectory("lib").CreateSubdirectory("app");
            CopyFiles(new DirectoryInfo(packDir), appDir, Utility.CreateProgressDelegate(progress, 0, 30));

            var metadataFiles = GetReleaseMetadataFiles();
            foreach (var kvp in metadataFiles) {
                File.Copy(kvp.Value, Path.Combine(stagingDir.FullName, kvp.Key), true);
            }

            AddContentTypesAndRel(nuspecPath);
            RenderReleaseNotesMarkdown(nuspecPath);

            await EasyZip.CreateZipFromDirectoryAsync(Log, outputPath, stagingDir.FullName, Utility.CreateProgressDelegate(progress, 30, 100));
            progress(100);
        }

        protected virtual Dictionary<string, string> GetReleaseMetadataFiles()
        {
            return new Dictionary<string, string>();
        }

        protected virtual void CopyFiles(DirectoryInfo source, DirectoryInfo target, Action<int> progress, bool excludeAnnoyances = false)
        {
            var numFiles = source.EnumerateFiles("*", SearchOption.AllDirectories).Count();
            int currentFile = 0;

            void CopyFilesInternal(DirectoryInfo source, DirectoryInfo target)
            {
                foreach (var fileInfo in source.GetFiles()) {
                    var path = Path.Combine(target.FullName, fileInfo.Name);
                    currentFile++;
                    progress((int) ((double) currentFile / numFiles * 100));
                    if (excludeAnnoyances && REGEX_EXCLUDES.IsMatch(path)) {
                        Log.Debug("Skipping because matched exclude pattern: " + path);
                        continue;
                    }
                    fileInfo.CopyTo(path, true);
                }

                foreach (var sourceSubDir in source.GetDirectories()) {
                    var targetSubDir = target.CreateSubdirectory(sourceSubDir.Name);
                    CopyFilesInternal(sourceSubDir, targetSubDir);
                }
            }

            CopyFilesInternal(source, target);
        }

        protected virtual void RenderReleaseNotesMarkdown(string specPath)
        {
            var doc = new XmlDocument();
            doc.Load(specPath);

            var metadata = doc.DocumentElement.ChildNodes
                .OfType<XmlElement>()
                .First(x => x.Name.ToLowerInvariant() == "metadata");

            var releaseNotes = metadata.ChildNodes
                .OfType<XmlElement>()
                .FirstOrDefault(x => x.Name.ToLowerInvariant() == "releasenotes");

            if (releaseNotes == null || String.IsNullOrWhiteSpace(releaseNotes.InnerText)) {
                Log.Debug($"No release notes found in {specPath}");
                return;
            }

            var releaseNotesHtml = doc.CreateElement("releaseNotesHtml");
            releaseNotesHtml.InnerText = String.Format("<![CDATA[\n" + "{0}\n" + "]]>",
                new Markdown().Transform(releaseNotes.InnerText));
            metadata.AppendChild(releaseNotesHtml);

            doc.Save(specPath);
        }

        protected virtual void AddContentTypesAndRel(string nuspecPath)
        {
            var rootDirectory = Path.GetDirectoryName(nuspecPath);
            var extensions = Directory.EnumerateFiles(rootDirectory, "*", SearchOption.AllDirectories)
                .Select(p => Path.GetExtension(p).TrimStart('.').ToLower())
                .Distinct()
                .Select(ext => $"""  <Default Extension="{ext}" ContentType="application/octet" />""")
                .ToArray();

            var contentType = $"""
<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml" />
{String.Join(Environment.NewLine, extensions)}
</Types>
""";

            File.WriteAllText(Path.Combine(rootDirectory, "[Content_Types].xml"), contentType);

            var relsDir = Path.Combine(rootDirectory, "_rels");
            Directory.CreateDirectory(relsDir);

            var rels = $"""
<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Type="http://schemas.microsoft.com/packaging/2010/07/manifest" Target="/{Path.GetFileName(nuspecPath)}" Id="R1" />
</Relationships>
""";
            File.WriteAllText(Path.Combine(relsDir, ".rels"), rels);
        }
    }
}
