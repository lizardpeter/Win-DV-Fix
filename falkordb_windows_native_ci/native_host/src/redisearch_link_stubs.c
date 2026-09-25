/*
 * TEMPORARY LINK-ONLY SHIM.
 *
 * FalkorDB's current Rust graph crate still declares RediSearch C ABI symbols.
 * For the native Windows bring-up we explicitly reject plans that require the
 * index backend, but the final linker still needs definitions for those externs.
 *
 * Every symbol below aborts if reached. This is safer than returning a fake
 * pointer/value and accidentally corrupting graph state. Remove this file once
 * graph/src/index/falkordb is fully wired as the native index backend.
 */
#include <stdio.h>
#include <stdlib.h>

#if defined(_MSC_VER)
#define NORETURN __declspec(noreturn)
#else
#define NORETURN __attribute__((noreturn))
#endif

static NORETURN void falkordb_unavailable(const char *name) {
    fprintf(stderr,
            "FalkorDB native Windows bring-up: unsupported RediSearch symbol reached: %s\n",
            name);
    fflush(stderr);
    abort();
}

#define STUB(name) NORETURN void name(void) { falkordb_unavailable(#name); }

STUB(RediSearch_CleanupModule)
STUB(RediSearch_CreateContainsNode)
STUB(RediSearch_CreateDocument)
STUB(RediSearch_CreateDocument2)
STUB(RediSearch_CreateEmptyNode)
STUB(RediSearch_CreateField)
STUB(RediSearch_CreateGeoNode)
STUB(RediSearch_CreateIndex)
STUB(RediSearch_CreateIndexOptions)
STUB(RediSearch_CreateIntersectNode)
STUB(RediSearch_CreateLexRangeNode)
STUB(RediSearch_CreateNotNode)
STUB(RediSearch_CreateNumericNode)
STUB(RediSearch_CreatePrefixNode)
STUB(RediSearch_CreateSuffixNode)
STUB(RediSearch_CreateTagContainsNode)
STUB(RediSearch_CreateTagLexRangeNode)
STUB(RediSearch_CreateTagNode)
STUB(RediSearch_CreateTagPrefixNode)
STUB(RediSearch_CreateTagSuffixNode)
STUB(RediSearch_CreateTagTokenNode)
STUB(RediSearch_CreateTokenNode)
STUB(RediSearch_CreateUnionNode)
STUB(RediSearch_CreateVecSimNode)
STUB(RediSearch_DeleteDocument)
STUB(RediSearch_DocumentAddField)
STUB(RediSearch_DocumentAddFieldGeo)
STUB(RediSearch_DocumentAddFieldNumber)
STUB(RediSearch_DocumentAddFieldNumericArray)
STUB(RediSearch_DocumentAddFieldString)
STUB(RediSearch_DocumentAddFieldStringArray)
STUB(RediSearch_DocumentAddFieldVector)
STUB(RediSearch_DocumentExists)
STUB(RediSearch_DropIndex)
STUB(RediSearch_ExportCapi)
STUB(RediSearch_FreeDocument)
STUB(RediSearch_FreeIndexOptions)
STUB(RediSearch_GC_total)
STUB(RediSearch_GetCApiVersion)
STUB(RediSearch_GetResultsIterator)
STUB(RediSearch_IndexAddDocument)
STUB(RediSearch_IndexClone)
STUB(RediSearch_IndexGetLanguage)
STUB(RediSearch_IndexGetScore)
STUB(RediSearch_IndexGetStopwords)
STUB(RediSearch_IndexInfo)
STUB(RediSearch_IndexInfoFree)
STUB(RediSearch_IndexOptionsSetFlags)
STUB(RediSearch_IndexOptionsSetGCPolicy)
STUB(RediSearch_IndexOptionsSetGetValueCallback)
STUB(RediSearch_IndexOptionsSetLanguage)
STUB(RediSearch_IndexOptionsSetScore)
STUB(RediSearch_IndexOptionsSetStopwords)
STUB(RediSearch_IndexRelease)
STUB(RediSearch_Init)
STUB(RediSearch_IterateQuery)
STUB(RediSearch_IterateQueryWithDialect)
STUB(RediSearch_MemUsage)
STUB(RediSearch_QueryNodeAddChild)
STUB(RediSearch_QueryNodeClearChildren)
STUB(RediSearch_QueryNodeFree)
STUB(RediSearch_QueryNodeGetChild)
STUB(RediSearch_QueryNodeGetFieldMask)
STUB(RediSearch_QueryNodeNumChildren)
STUB(RediSearch_QueryNodeType)
STUB(RediSearch_ResultsIteratorFree)
STUB(RediSearch_ResultsIteratorGetScore)
STUB(RediSearch_ResultsIteratorNext)
STUB(RediSearch_ResultsIteratorReset)
STUB(RediSearch_SetCriteriaTesterThreshold)
STUB(RediSearch_SetDefaultScorer)
STUB(RediSearch_SetNumWorkerThreads)
STUB(RediSearch_StopwordsList_Contains)
STUB(RediSearch_StopwordsList_Free)
STUB(RediSearch_TagFieldSetCaseSensitive)
STUB(RediSearch_TagFieldSetSeparator)
STUB(RediSearch_TextFieldSetWeight)
STUB(RediSearch_TotalInfo)
STUB(RediSearch_TotalMemUsage)
STUB(RediSearch_ValidateLanguage)
STUB(RediSearch_VecSimTieredParams_Init)
STUB(RediSearch_VectorFieldSetParams)
