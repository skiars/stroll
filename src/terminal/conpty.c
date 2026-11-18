#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0A00
#endif
#include <windows.h>
#include <stdint.h>

typedef struct {
    HPCON console;
    HANDLE job;
    HANDLE process;
} ReaderConPty;

DWORD reader_conpty_open(ReaderConPty *pty, HANDLE *input, HANDLE *output, wchar_t *command,
                         const wchar_t *directory, uint16_t cols, uint16_t rows) {
    HANDLE input_read = NULL, input_write = NULL, output_read = NULL, output_write = NULL;
    LPPROC_THREAD_ATTRIBUTE_LIST attributes = NULL;
    BOOL initialized = FALSE;
    DWORD error = ERROR_SUCCESS;
    if (!CreatePipe(&input_read, &input_write, NULL, 0) ||
        !CreatePipe(&output_read, &output_write, NULL, 0)) goto fail;

    HRESULT result = CreatePseudoConsole((COORD){(SHORT)cols, (SHORT)rows},
                                        input_read, output_write, 0, &pty->console);
    if (FAILED(result)) {
        error = HRESULT_FACILITY(result) == FACILITY_WIN32 ? HRESULT_CODE(result) : ERROR_GEN_FAILURE;
        goto cleanup;
    }
    CloseHandle(input_read);
    input_read = NULL;
    CloseHandle(output_write);
    output_write = NULL;

    pty->job = CreateJobObjectW(NULL, NULL);
    if (!pty->job) goto fail;
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits = {0};
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if (!SetInformationJobObject(pty->job, JobObjectExtendedLimitInformation,
                                 &limits, sizeof(limits))) goto fail;

    SIZE_T size = 0;
    InitializeProcThreadAttributeList(NULL, 1, 0, &size);
    attributes = HeapAlloc(GetProcessHeap(), 0, size);
    if (!attributes) {
        error = ERROR_OUTOFMEMORY;
        goto cleanup;
    }
    if (!InitializeProcThreadAttributeList(attributes, 1, 0, &size)) goto fail;
    initialized = TRUE;
    if (!UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                                   pty->console, sizeof(pty->console), NULL, NULL)) goto fail;

    STARTUPINFOEXW startup = {0};
    startup.StartupInfo.cb = sizeof(startup);
    // Use ConPTY's console streams even if Stroll's output is redirected.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.lpAttributeList = attributes;
    PROCESS_INFORMATION child = {0};
    // App execution aliases can reject a job list during process creation.
    // Assign the suspended child before it can start any descendants.
    if (!CreateProcessW(NULL, command, NULL, NULL, FALSE,
                        EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                        NULL, directory, &startup.StartupInfo, &child)) goto fail;
    if (!AssignProcessToJobObject(pty->job, child.hProcess) ||
        ResumeThread(child.hThread) == (DWORD)-1) {
        error = GetLastError();
        TerminateProcess(child.hProcess, error);
        CloseHandle(child.hThread);
        CloseHandle(child.hProcess);
        goto cleanup;
    }
    pty->process = child.hProcess;
    CloseHandle(child.hThread);
    *input = input_write;
    *output = output_read;
    goto cleanup;

fail:
    error = GetLastError();
cleanup:
    if (initialized) DeleteProcThreadAttributeList(attributes);
    if (attributes) HeapFree(GetProcessHeap(), 0, attributes);
    if (input_read) CloseHandle(input_read);
    if (output_write) CloseHandle(output_write);
    if (error) {
        if (pty->job) CloseHandle(pty->job);
        // Close the pipe ends before closing an unused console. During normal
        // shutdown the reader thread drains the pipe while ConPTY closes.
        if (input_write) CloseHandle(input_write);
        if (output_read) CloseHandle(output_read);
        if (pty->console) ClosePseudoConsole(pty->console);
    }
    return error;
}

DWORD reader_conpty_resize(ReaderConPty *pty, uint16_t cols, uint16_t rows) {
    HRESULT result = ResizePseudoConsole(pty->console, (COORD){(SHORT)cols, (SHORT)rows});
    if (SUCCEEDED(result)) return ERROR_SUCCESS;
    return HRESULT_FACILITY(result) == FACILITY_WIN32 ? HRESULT_CODE(result) : ERROR_GEN_FAILURE;
}

BOOL reader_conpty_exited(ReaderConPty *pty, DWORD *status) {
    return WaitForSingleObject(pty->process, 0) == WAIT_OBJECT_0 &&
           GetExitCodeProcess(pty->process, status);
}

void reader_conpty_close(ReaderConPty *pty) {
    CloseHandle(pty->job);
    ClosePseudoConsole(pty->console);
    WaitForSingleObject(pty->process, INFINITE);
    CloseHandle(pty->process);
}
