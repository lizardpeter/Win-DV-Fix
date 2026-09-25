param(
    [Parameter(Mandatory=$true)][string]$OutDir,
    [string]$VSGenerator = ""
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($VSGenerator)) {
    $Help = (cmake --help | Out-String)
    $VSGenerator = @("Visual Studio 18 2026", "Visual Studio 17 2022") |
        Where-Object { $Help.Contains($_) } |
        Select-Object -First 1
    if (-not $VSGenerator) {
        throw "No supported Visual Studio CMake generator found"
    }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$Source = Join-Path $OutDir "redisearch_shim.c"
$CMakeLists = Join-Path $OutDir "CMakeLists.txt"
$BuildDir = Join-Path $OutDir "build"
$Archive = Join-Path $OutDir "redisearch_shim.lib"

$Functions = @(
"RediSearch_CleanupModule","RediSearch_CreateContainsNode","RediSearch_CreateDocument",
"RediSearch_CreateDocument2","RediSearch_CreateEmptyNode","RediSearch_CreateField",
"RediSearch_CreateGeoNode","RediSearch_CreateIndex","RediSearch_CreateIndexOptions",
"RediSearch_CreateIntersectNode","RediSearch_CreateLexRangeNode","RediSearch_CreateNotNode",
"RediSearch_CreateNumericNode","RediSearch_CreatePrefixNode","RediSearch_CreateSuffixNode",
"RediSearch_CreateTagContainsNode","RediSearch_CreateTagLexRangeNode","RediSearch_CreateTagNode",
"RediSearch_CreateTagPrefixNode","RediSearch_CreateTagSuffixNode","RediSearch_CreateTagTokenNode",
"RediSearch_CreateTokenNode","RediSearch_CreateUnionNode","RediSearch_CreateVecSimNode",
"RediSearch_DeleteDocument","RediSearch_DocumentAddField","RediSearch_DocumentAddFieldGeo",
"RediSearch_DocumentAddFieldNumber","RediSearch_DocumentAddFieldNumericArray",
"RediSearch_DocumentAddFieldString","RediSearch_DocumentAddFieldStringArray",
"RediSearch_DocumentAddFieldVector","RediSearch_DocumentExists","RediSearch_DropIndex",
"RediSearch_ExportCapi","RediSearch_FreeDocument","RediSearch_FreeIndexOptions",
"RediSearch_GC_total","RediSearch_GetCApiVersion","RediSearch_GetResultsIterator",
"RediSearch_IndexAddDocument","RediSearch_IndexClone","RediSearch_IndexGetLanguage",
"RediSearch_IndexGetScore","RediSearch_IndexGetStopwords","RediSearch_IndexInfo",
"RediSearch_IndexInfoFree","RediSearch_IndexOptionsSetFlags","RediSearch_IndexOptionsSetGCPolicy",
"RediSearch_IndexOptionsSetGetValueCallback","RediSearch_IndexOptionsSetLanguage",
"RediSearch_IndexOptionsSetScore","RediSearch_IndexOptionsSetStopwords",
"RediSearch_IndexRelease","RediSearch_Init","RediSearch_IterateQuery",
"RediSearch_IterateQueryWithDialect","RediSearch_MemUsage","RediSearch_QueryNodeAddChild",
"RediSearch_QueryNodeClearChildren","RediSearch_QueryNodeFree","RediSearch_QueryNodeGetChild",
"RediSearch_QueryNodeGetFieldMask","RediSearch_QueryNodeNumChildren","RediSearch_QueryNodeType",
"RediSearch_ResultsIteratorFree","RediSearch_ResultsIteratorGetScore",
"RediSearch_ResultsIteratorNext","RediSearch_ResultsIteratorReset",
"RediSearch_SetCriteriaTesterThreshold","RediSearch_SetDefaultScorer",
"RediSearch_SetNumWorkerThreads","RediSearch_StopwordsList_Contains",
"RediSearch_StopwordsList_Free","RediSearch_TagFieldSetCaseSensitive",
"RediSearch_TagFieldSetSeparator","RediSearch_TextFieldSetWeight",
"RediSearch_TotalInfo","RediSearch_TotalMemUsage","RediSearch_ValidateLanguage",
"RediSearch_VecSimTieredParams_Init","RediSearch_VectorFieldSetParams"
)

$Lines = @(
'#include <stdlib.h>',
'#if defined(_MSC_VER)',
'#  define RS_NORETURN __declspec(noreturn)',
'#else',
'#  define RS_NORETURN __attribute__((noreturn))',
'#endif',
'',
'void *RSDummyContext = 0;',
'RS_NORETURN static void falkordb_native_index_disabled(void) { abort(); }',
''
)
foreach ($Name in $Functions) {
    $Lines += "RS_NORETURN void $Name(void) { falkordb_native_index_disabled(); }"
}
Set-Content -Path $Source -Value $Lines -Encoding Ascii

@'
cmake_minimum_required(VERSION 3.20)
project(redisearch_shim LANGUAGES C)
add_library(redisearch_shim STATIC redisearch_shim.c)
set_target_properties(redisearch_shim PROPERTIES OUTPUT_NAME redisearch_shim)
'@ | Set-Content -Path $CMakeLists -Encoding Ascii

cmake -S $OutDir -B $BuildDir -G "$VSGenerator" -A x64
cmake --build $BuildDir --config Release --target redisearch_shim

$Built = Join-Path $BuildDir "Release\redisearch_shim.lib"
if (-not (Test-Path $Built)) {
    throw "Expected RediSearch shim not found: $Built"
}
Copy-Item -Force $Built $Archive
Write-Host "Built fail-closed linker shim: $Archive"
