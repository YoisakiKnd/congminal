/* Isolated test peer. No SSH, network, user files or credentials. */
#define _WIN32_WINNT 0x0a00
#include <windows.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <wchar.h>
_Static_assert(sizeof(SECURITY_ATTRIBUTES) == 24, "security ABI");
_Static_assert(sizeof(STARTUPINFOW) == 104, "startup ABI");
_Static_assert(sizeof(STARTUPINFOEXW) == 112, "extended startup ABI");
_Static_assert(sizeof(PROCESS_INFORMATION) == 24, "process ABI");
_Static_assert(sizeof(OVERLAPPED) == 32, "overlapped ABI");
_Static_assert(sizeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION) == 144, "job limit ABI");
_Static_assert(sizeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION) == 48, "accounting ABI");
_Static_assert(offsetof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, ActiveProcesses) == 40, "active offset");
_Static_assert(PROC_THREAD_ATTRIBUTE_HANDLE_LIST == 0x20002, "handle attribute");
_Static_assert(PROC_THREAD_ATTRIBUTE_JOB_LIST == 0x2000d, "job attribute");
_Static_assert(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE == 0x2000, "job flag");
_Static_assert(JobObjectExtendedLimitInformation == 9, "limit class");
_Static_assert(JobObjectBasicAccountingInformation == 1, "accounting class");
_Static_assert((CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT) == 0x08080400, "creation flags");
static int output(const void *bytes, DWORD length) {
    const unsigned char *cursor = bytes;
    while (length) {
        DWORD written = 0;
        if (!WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), cursor, length, &written, NULL) || !written) return 0;
        cursor += written; length -= written;
    }
    return 1;
}
int wmain(int argc, wchar_t **argv) {
    BOOL in_job = FALSE;
    if (argc < 2 || !IsProcessInJob(GetCurrentProcess(), NULL, &in_job) || !in_job) return 69;
    if (!wcscmp(argv[1], L"argv")) {
        DWORD count = (DWORD)(argc - 2);
        if (!output("CGA1", 4) || !output(&count, 4)) return 74;
        for (int index = 2; index < argc; ++index) {
            DWORD units = (DWORD)wcslen(argv[index]);
            if (units > 32766 || !output(&units, 4) || !output(argv[index], units * 2)) return 74;
        }
        return 0;
    }
    if (!wcscmp(argv[1], L"emit")) {
        unsigned char bytes[4096];
        for (unsigned index = 0; index < sizeof(bytes); ++index) bytes[index] = (unsigned char)(index % 251);
        return output(bytes, sizeof(bytes)) ? 7 : 74;
    }
    if (!wcscmp(argv[1], L"flood")) {
        const unsigned char bytes[4096] = {0};
        for (int index = 0; index < 128; ++index) if (!output(bytes, sizeof(bytes))) return 74;
        return 0;
    }
    if (!wcscmp(argv[1], L"echo")) {
        unsigned char bytes[8192];
        for (;;) {
            DWORD count = 0;
            if (!ReadFile(GetStdHandle(STD_INPUT_HANDLE), bytes, sizeof(bytes), &count, NULL)) return 74;
            if (!count) return 0;
            if (!output(bytes, count)) return 74;
        }
    }
    if (!wcscmp(argv[1], L"wait")) {
        if (argc == 3) {
            HANDLE marker = CreateFileW(argv[2], GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, NULL);
            if (marker == INVALID_HANDLE_VALUE) return 75;
            CloseHandle(marker);
        }
        Sleep(30000); return 0;
    }
    if (!wcscmp(argv[1], L"parent")) {
        if (argc != 3) return 64;
        wchar_t program[4096], line[8192];
        DWORD length = GetModuleFileNameW(NULL, program, 4096);
        if (!length || length >= 4096 || swprintf(line, 8192, L"\"%ls\" wait \"%ls\"", program, argv[2]) < 0) return 76;
        STARTUPINFOW startup = {0}; PROCESS_INFORMATION child = {0};
        startup.cb = sizeof(startup); startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE); startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
        startup.hStdError = GetStdHandle(STD_ERROR_HANDLE);
        if (!CreateProcessW(program, line, NULL, NULL, TRUE, CREATE_NO_WINDOW, NULL, NULL, &startup, &child)) return 77;
        DWORD pid = child.dwProcessId;
        ULONGLONG until = GetTickCount64() + 5000;
        while (GetFileAttributesW(argv[2]) == INVALID_FILE_ATTRIBUTES && GetTickCount64() < until && WaitForSingleObject(child.hProcess, 0) == WAIT_TIMEOUT) Sleep(1);
        BOOL ready = GetFileAttributesW(argv[2]) != INVALID_FILE_ATTRIBUTES;
        BOOL written = output(&pid, 4);
        CloseHandle(child.hThread); CloseHandle(child.hProcess);
        /* The descendant retains stdout. The supervisor must terminate the job to see EOF. */
        return written && ready ? 0 : 78;
    }
    return 64;
}
