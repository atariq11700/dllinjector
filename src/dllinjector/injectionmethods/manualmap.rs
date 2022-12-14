use crate::utils;
use winapi::{
    shared::{
        basetsd::{SIZE_T, ULONG_PTR},
        minwindef::{BOOL, DWORD, FARPROC, HINSTANCE, HMODULE, LPCVOID, LPDWORD, LPVOID, WORD},
        ntdef::{HANDLE, LPCSTR},
    },
    um::{
        handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
        libloaderapi::{GetModuleHandleA, GetProcAddress},
        memoryapi::{VirtualAllocEx, VirtualFreeEx, WriteProcessMemory},
        minwinbase::LPSECURITY_ATTRIBUTES,
        processthreadsapi::{CreateRemoteThreadEx, OpenProcess, LPPROC_THREAD_ATTRIBUTE_LIST},
        tlhelp32::PROCESSENTRY32,
        winnt::{
            IMAGE_IMPORT_DESCRIPTOR_u, DLL_PROCESS_ATTACH, IMAGE_BASE_RELOCATION,
            IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_IMPORT,
            IMAGE_DIRECTORY_ENTRY_TLS, IMAGE_DOS_HEADER, IMAGE_FILE_HEADER, IMAGE_IMPORT_BY_NAME,
            IMAGE_IMPORT_DESCRIPTOR, IMAGE_NT_HEADERS, IMAGE_OPTIONAL_HEADER, IMAGE_TLS_DIRECTORY,
            MEM_COMMIT, MEM_FREE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PIMAGE_NT_HEADERS,
            PIMAGE_SECTION_HEADER, PIMAGE_TLS_CALLBACK, PROCESS_ALL_ACCESS, PVOID,
        },
    },
    vc::vadefs::uintptr_t,
};

#[cfg(target_pointer_width = "64")]
use winapi::um::winnt::{IMAGE_ORDINAL_FLAG64, IMAGE_REL_BASED_DIR64};

#[cfg(target_pointer_width = "32")]
use winapi::um::winnt::{IMAGE_ORDINAL_FLAG32, IMAGE_REL_BASED_HIGHLOW};

//function pointer types
#[allow(non_camel_case_types)]
type f_LoadLibraryA = unsafe extern "system" fn(lpLibraryFilename: LPCSTR) -> HINSTANCE;
#[allow(non_camel_case_types)]
type f_GetProcAddress = unsafe extern "system" fn(hModule: HMODULE, lpProcName: LPCSTR) -> FARPROC;
#[allow(non_camel_case_types)]
type f_DllMain = unsafe extern "system" fn(
    hModule: HMODULE,
    dw_reason_for_call: DWORD,
    lpReserved: LPVOID,
) -> BOOL;

///Data struct to be populated and passed to the loader function inside target process
struct ManualMapLoaderData {
    p_load_library_a: f_LoadLibraryA,
    p_get_proc_address: f_GetProcAddress,
}

/// rust implementation of the cpp IMAGE_FIRST_SECTION macro
fn image_first_section(pnt_header: PIMAGE_NT_HEADERS) -> PIMAGE_SECTION_HEADER {
    let base = pnt_header as ULONG_PTR;
    let off1 = memoffset::offset_of!(IMAGE_NT_HEADERS, OptionalHeader);
    let off2 = ((unsafe { *pnt_header } as IMAGE_NT_HEADERS)
        .FileHeader
        .SizeOfOptionalHeader) as usize;
    return (base + off1 + off2) as PIMAGE_SECTION_HEADER;
}

///wrapper for the pe image section names so that they can be printed nicely
//.text
//.rdata
//.bss
//etc...
struct SectionName {
    bytes: [u8; 8],
}
impl SectionName {
    fn from(name_array: [u8; 8]) -> SectionName {
        return SectionName { bytes: name_array };
    }
}
impl std::fmt::Display for SectionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for i in 0..8 {
            match write!(f, "{}", self.bytes[i] as char) {
                Ok(_) => {}
                Err(err) => {
                    println!("Error printing section name {err}")
                }
            }
        }
        Ok(())
    }
}

///takes a windows dll as a *const u8 to the bytes and returns refernces to the DOS, NT, OPTIONAL, and FILE headers
fn get_headers_from_dll<'a>(
    p_data: *const u8,
) -> (
    &'a IMAGE_DOS_HEADER,
    &'a IMAGE_NT_HEADERS,
    &'a IMAGE_OPTIONAL_HEADER,
    &'a IMAGE_FILE_HEADER,
) {
    let dos_header: &IMAGE_DOS_HEADER = unsafe { &*(p_data as *const IMAGE_DOS_HEADER) };
    let nt_header = unsafe {
        &*(p_data.add(dos_header.e_lfanew.try_into().unwrap()) as *const IMAGE_NT_HEADERS)
    };
    let optional_header = &nt_header.OptionalHeader;
    let file_header = &nt_header.FileHeader;

    return (dos_header, nt_header, optional_header, file_header);
}

///Manual Map injection function
///
/// Reads in and validates the dll. Then opens the target process and allocates/writes the dll sections, the dll pe headers, the loader function, and the data for the loader function. It then creates a remote thread calling the loader function
pub fn inject(proc: PROCESSENTRY32, dll_path: String) -> bool {
    //read in and validate dll
    let dll_data = utils::files::is_valid_dll(dll_path.clone());
    if !(dll_data.len() > 0) {
        println!("Unable to read dll");
        return false;
    }

    println!(
        "Dll loaded in host process at 0x{:x}",
        dll_data.as_ptr() as usize
    );

    //open target process
    let target_proc: HANDLE =
        unsafe { OpenProcess(PROCESS_ALL_ACCESS, false as BOOL, proc.th32ProcessID) };

    if target_proc == INVALID_HANDLE_VALUE {
        println!("Unable to open target process");
        return false;
    }

    println!(
        "Opened process [{}] {}, Handle: 0x{:x}",
        proc.th32ProcessID,
        crate::dllinjector::components::processeslist::sz_exe_to_string(proc.szExeFile),
        target_proc as usize
    );

    //get the dll headers
    let (dos_header, nt_header, optional_header, file_header) =
        get_headers_from_dll(dll_data.as_ptr());

    println!(
        "Dll dos header in host process found at 0x{:x}",
        dos_header as *const IMAGE_DOS_HEADER as usize
    );
    println!(
        "Dll nt header in host process found at 0x{:x}",
        nt_header as *const IMAGE_NT_HEADERS as usize
    );
    println!(
        "Dll optional header in host process found at 0x{:x}",
        optional_header as *const IMAGE_OPTIONAL_HEADER as usize
    );

    println!(
        "Dll file header in host process found at 0x{:x}",
        file_header as *const IMAGE_FILE_HEADER as usize
    );

    //allocate enough memory inside the target process for the dll sections/code
    let mut base_addr_ex = unsafe {
        VirtualAllocEx(
            target_proc,
            optional_header.ImageBase as LPVOID,
            optional_header.SizeOfImage as SIZE_T,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_EXECUTE_READWRITE,
        ) as *mut u8
    };

    if base_addr_ex as usize == 0 {
        base_addr_ex = unsafe {
            VirtualAllocEx(
                target_proc,
                0 as LPVOID,
                optional_header.SizeOfImage as SIZE_T,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READWRITE,
            ) as *mut u8
        };
    }

    if base_addr_ex as usize == 0 {
        println!("Unable to allocate memory inside target process for dll");
        unsafe { CloseHandle(target_proc) };
    }
    println!(
        "Allocated 0x{:x} bytes in target proc at 0x{:x}",
        optional_header.SizeOfImage, base_addr_ex as usize
    );

    let mut psection_header =
        image_first_section(nt_header as *const IMAGE_NT_HEADERS as PIMAGE_NT_HEADERS);

    //Write the dll sections to the target process
    for _ in 0..file_header.NumberOfSections {
        unsafe {
            let section_header = &*psection_header;
            println!(
                "Found section header {} at 0x{:x}",
                SectionName::from(section_header.Name),
                psection_header as usize
            );
            //write the section so long as it has a size > 0
            if section_header.SizeOfRawData > 0 {
                let name = section_header.Name;
                if WriteProcessMemory(
                    target_proc,
                    base_addr_ex.add(section_header.VirtualAddress as usize) as LPVOID,
                    dll_data
                        .as_ptr()
                        .add(section_header.PointerToRawData as usize)
                        as LPCVOID,
                    section_header.SizeOfRawData as SIZE_T,
                    0 as *mut usize,
                ) == 0
                {
                    println!(
                        "Unable to map section {} into target process memory",
                        SectionName::from(name)
                    );
                    CloseHandle(target_proc);
                    VirtualFreeEx(
                        target_proc,
                        base_addr_ex as LPVOID,
                        optional_header.SizeOfImage as SIZE_T,
                        MEM_FREE,
                    );
                    return false;
                }
                println!(
                    "Mapped dll section {} ({}) into target process as 0x{:x}",
                    SectionName::from(name),
                    section_header.SizeOfRawData,
                    base_addr_ex.add(section_header.VirtualAddress as usize) as usize
                );
            }
            psection_header = psection_header.add(1);
        }
    }

    //write the first 0x1000 bytes of the pe headers to the target process
    //would be better to calculate the actual size
    if unsafe {
        WriteProcessMemory(
            target_proc,
            base_addr_ex as LPVOID,
            dll_data.as_ptr() as LPCVOID,
            0x1000,
            0 as *mut SIZE_T,
        )
    } == 0
    {
        println!("Unable to write pe headers to target process");
    }
    println!("Wrote pe headers to target process");

    //setup the loader data
    //get functions pointers to LoadLibraryA and GetProcAddress. The function pointers need to point to the functions withing kernel32.dll so use GetProcAddress to get the correct addresss
    let kernel32 = unsafe { GetModuleHandleA("kernel32.dll\0".as_ptr() as LPCSTR) };
    let mm_data = ManualMapLoaderData {
        p_load_library_a: unsafe {
            std::mem::transmute(GetProcAddress(
                kernel32,
                "LoadLibraryA\0".as_ptr() as LPCSTR,
            ))
        },
        p_get_proc_address: unsafe {
            std::mem::transmute(GetProcAddress(
                kernel32,
                "GetProcAddress\0".as_ptr() as LPCSTR,
            ))
        }
    };

    //write the loader data
    if unsafe {
        WriteProcessMemory(
            target_proc,
            base_addr_ex as LPVOID,
            &mm_data as *const ManualMapLoaderData as LPCVOID,
            std::mem::size_of::<ManualMapLoaderData>(),
            0 as *mut SIZE_T,
        )
    } == 0
    {
        println!("Unable to write loader data");
    }
    println!("Wrote loader data to target process");

    //allocate 0x1000 bytes for the loader function within the target process
    let loader_addr = unsafe {
        VirtualAllocEx(
            target_proc,
            0 as LPVOID,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_EXECUTE_READWRITE,
        )
    };
    if loader_addr as usize == 0 {
        println!("Unable to allocate data in target process for oader funtion");
    }
    println!(
        "Allocated 0x1000 bytes at 0x{:x} inside the target process for the loader function",
        loader_addr as usize
    );

    //write the loader function to the target process
    if unsafe {
        WriteProcessMemory(
            target_proc,
            loader_addr,
            loader as LPCVOID,
            0x1000,
            0 as *mut SIZE_T,
        )
    } == 0
    {
        println!("Unable to write loader function to the target process");
    }
    println!("Wrote loader function to the target process");

    //create a remote thread withing the target process and call the loader function
    if unsafe {
        CreateRemoteThreadEx(
            target_proc,
            0 as LPSECURITY_ATTRIBUTES,
            0,
            std::mem::transmute(loader_addr),
            base_addr_ex as LPVOID,
            0,
            0 as LPPROC_THREAD_ATTRIBUTE_LIST,
            0 as LPDWORD,
        )
    } == INVALID_HANDLE_VALUE
    {
        println!("Unable to create remote thread inside the target process");
    }
    println!("Created remote thread inside the target process");

    return true;
}

unsafe extern "system" fn loader(pmm_data: *mut ManualMapLoaderData) {
    //make sure the base address and data is a valid pointer
    if pmm_data as usize == 0 {
        return;
    }

    //turn the function pointers back into functions for use later
    #[allow(non_snake_case)]
    let _LoadLibraryA = (*pmm_data).p_load_library_a;
    #[allow(non_snake_case)]
    let _GetProcAddress = &(*pmm_data).p_get_proc_address;


    let base_addr = pmm_data as *const u8;

    //get the dll headers again
    let dos_header: &IMAGE_DOS_HEADER = &*(base_addr as *const IMAGE_DOS_HEADER);
    let nt_header = &*(base_addr.add(dos_header.e_lfanew as usize) as *const IMAGE_NT_HEADERS);
    let optional_header = &nt_header.OptionalHeader;
    let _file_header = &nt_header.FileHeader;

    #[allow(non_snake_case)]
    let _DllMain: f_DllMain =
        std::mem::transmute(base_addr.add(optional_header.AddressOfEntryPoint as usize));

    //perform relocations if necessary
    let loc_delta = base_addr as u64 - optional_header.ImageBase;
    if loc_delta != 0 {
        //make sure the dll has a valid relocation section
        if optional_header.DataDirectory[IMAGE_DIRECTORY_ENTRY_BASERELOC as usize].Size == 0 {
            return;
        }

        let mut preloc_data: *mut IMAGE_BASE_RELOCATION = base_addr.add(
            optional_header.DataDirectory[IMAGE_DIRECTORY_ENTRY_BASERELOC as usize].VirtualAddress
                as usize,
        ) as *mut IMAGE_BASE_RELOCATION;
        let reloc_data = &*preloc_data;

        while reloc_data.VirtualAddress != 0 {
            let number_of_entries = (reloc_data.SizeOfBlock as usize
                - std::mem::size_of::<IMAGE_BASE_RELOCATION>())
                / std::mem::size_of::<WORD>();
            let mut prelative_info = preloc_data.add(1) as *const WORD;

            for _ in 0..number_of_entries {
                #[cfg(target_pointer_width = "64")]
                if (*prelative_info >> 0x0C) == IMAGE_REL_BASED_DIR64 {
                    let p_patch = base_addr
                        .add(reloc_data.VirtualAddress as usize)
                        .add((*prelative_info & 0xFFF) as usize)
                        as *mut uintptr_t;
                    *p_patch += loc_delta as usize;
                }

                #[cfg(target_pointer_width = "32")]
                if (*prelative_info >> 0x0C) == IMAGE_REL_BASED_HIGHLOW {
                    let p_patch = base_addr
                        .add(reloc_data.VirtualAddress as usize)
                        .add((*prelative_info & 0xFFF) as usize)
                        as *mut uintptr_t;
                    *p_patch += loc_delta as usize;
                }
                prelative_info = prelative_info.add(1);
            }

            preloc_data = (preloc_data as *mut u8).add(reloc_data.SizeOfBlock as usize)
                as *mut IMAGE_BASE_RELOCATION;
        }
    }

    //check the IAT for imports
    if optional_header.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize].Size != 0 {
        let mut pimport_desc = base_addr.add(
            optional_header.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize].VirtualAddress
                as usize,
        ) as *const IMAGE_IMPORT_DESCRIPTOR;
        let mut import_desc = &*pimport_desc;

        //import the functions and modules if they are in the IAT
        while import_desc.Name != 0 {
            let sz_module = base_addr.add(import_desc.Name as usize) as *const i8;

            let loaded_module = _LoadLibraryA(sz_module);

            let original_first_thunk =
                *(&import_desc.u as *const IMAGE_IMPORT_DESCRIPTOR_u as *const usize);
            let mut p_thunk = base_addr.add(original_first_thunk) as *mut uintptr_t;

            let mut p_func = base_addr.add(import_desc.FirstThunk as usize) as *mut uintptr_t;

            if p_thunk as usize != 0 {
                p_thunk = p_func;
            }

            while *p_thunk != 0 {
                #[cfg(target_pointer_width = "64")]
                if ((*p_thunk as u64) & IMAGE_ORDINAL_FLAG64) != 0 {
                    *p_func =
                        _GetProcAddress(loaded_module, (*p_thunk & 0xFFFF) as *const i8) as usize;
                } else {
                    let import_name = base_addr.add(*p_thunk) as *const IMAGE_IMPORT_BY_NAME;
                    *p_func =
                        _GetProcAddress(loaded_module, &(*import_name).Name[0] as LPCSTR) as usize;
                }

                #[cfg(target_pointer_width = "32")]
                if ((*p_thunk as u64) & IMAGE_ORDINAL_FLAG32) != 0 {
                    *p_func =
                        _GetProcAddress(loaded_module, (*p_thunk & 0xFFFF) as *const i8) as usize;
                } else {
                    let import_name = base_addr.add(*p_thunk) as *const IMAGE_IMPORT_BY_NAME;
                    *p_func =
                        _GetProcAddress(loaded_module, &(*import_name).Name[0] as LPCSTR) as usize;
                }

                p_thunk = p_thunk.add(1);
                p_func = p_func.add(1);
            }
            pimport_desc = pimport_desc.add(1);
            import_desc = &*pimport_desc;
        }
    }

    //call the necessary TLS callbacks
    if optional_header.DataDirectory[IMAGE_DIRECTORY_ENTRY_TLS as usize].Size != 0 {
        let p_tls_dir = base_addr.add(
            optional_header.DataDirectory[IMAGE_DIRECTORY_ENTRY_TLS as usize].VirtualAddress
                as usize,
        ) as *const IMAGE_TLS_DIRECTORY;

        let mut p_tls_callback = (*p_tls_dir).AddressOfCallBacks as *const PIMAGE_TLS_CALLBACK;

        while p_tls_callback as usize != 0 {
            match *p_tls_callback {
                Some(callback) => callback(base_addr as PVOID, DLL_PROCESS_ATTACH, 0 as PVOID),
                None => break,
            }
            p_tls_callback = p_tls_callback.add(1);
        }
    }

    //call the dll main
    _DllMain(base_addr as HMODULE, DLL_PROCESS_ATTACH, 0 as PVOID);
}
