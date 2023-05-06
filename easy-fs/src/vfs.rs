use super::{
    block_cache_sync_all, get_block_cache, BlockDevice, DirEntry, DiskInode, DiskInodeType,
    EasyFileSystem, DIRENT_SZ,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
    inode_id: u32,
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
        inode_id: u32,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
            inode_id,
        }
    }
    /// Call a function over a disk inode to read it
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }
    /// Find inode under a disk inode by name
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                DIRENT_SZ,
            );
            if dirent == DirEntry::empty() {
                continue;
            }
            if dirent.name() == name {
                return Some(dirent.inode_id() as u32);
            }
        }
        None
    }
    /// Find inode under current inode by name
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                    inode_id,
                ))
            })
        })
    }
    /// Increase the size of a disk inode
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
    /// Create inode under current inode by name
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let mut fs = self.fs.lock();
        let op = |root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() {
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let mut i = 0;
            while i < file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    root_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                if dirent == DirEntry::empty() {
                    break;
                }
                i += 1;
            }
            let dirent = DirEntry::new(name, new_inode_id);
            // no empty dir entry
            if i >= file_count {
                let new_size = (file_count + 1) * DIRENT_SZ;
                // increase size
                self.increase_size(new_size as u32, root_inode, &mut fs);
            }
            // write dirent
            root_inode.write_at(i * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
        });

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        // return inode
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
            new_inode_id,
        )))
        // release efs lock automatically by compiler
    }
    /// List inodes under current inode
    pub fn ls(&self) -> Vec<String> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                v.push(String::from(dirent.name()));
            }
            v
        })
    }
    /// Hard link new_name to old_name
    pub fn linkat(&self, old_name: &str, new_name: &str) -> bool {
        if old_name == new_name {
            return false;
        }
        let mut fs = self.fs.lock();
        let old_inode_id =
            self.read_disk_inode(|disk_inode| self.find_inode_id(old_name, disk_inode));
        match old_inode_id {
            Some(inode_id) => {
                self.modify_disk_inode(|root_inode| {
                    // append file in the dirent
                    let file_count = (root_inode.size as usize) / DIRENT_SZ;
                    let mut i = 0;
                    while i < file_count {
                        let mut dirent = DirEntry::empty();
                        assert_eq!(
                            root_inode.read_at(
                                i * DIRENT_SZ,
                                dirent.as_bytes_mut(),
                                &self.block_device,
                            ),
                            DIRENT_SZ,
                        );
                        if dirent == DirEntry::empty() {
                            break;
                        }
                        i += 1;
                    }
                    let dirent = DirEntry::new(new_name, inode_id);
                    // no empty dir entry
                    if i >= file_count {
                        let new_size = (file_count + 1) * DIRENT_SZ;
                        // increase size
                        self.increase_size(new_size as u32, root_inode, &mut fs);
                    }
                    // write dirent
                    root_inode.write_at(i * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
                });
                block_cache_sync_all();
                true
            }
            None => false,
        }
    }
    /// Cancel hard link by name
    pub fn unlinkat(&self, name: &str) -> bool {
        // let mut _fs = self.fs.lock();
        let inode_id = self.read_disk_inode(|disk_inode| self.find_inode_id(name, disk_inode));
        match inode_id {
            Some(inode_id) => {
                let nlink = self.get_nlink(inode_id);
                if nlink > 0 {
                    // TODO: delete DirEntry
                    self.modify_disk_inode(|disk_inode| {
                        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
                        for i in 0..file_count {
                            let mut dirent = DirEntry::empty();
                            assert_eq!(
                                disk_inode.read_at(
                                    i * DIRENT_SZ,
                                    dirent.as_bytes_mut(),
                                    &self.block_device,
                                ),
                                DIRENT_SZ,
                            );
                            if dirent == DirEntry::empty() {
                                continue;
                            }
                            if dirent.name() == name {
                                let empty = DirEntry::empty();
                                disk_inode.write_at(
                                    i * DIRENT_SZ,
                                    empty.as_bytes(),
                                    &self.block_device,
                                );
                                break;
                            }
                        }
                    });
                    if nlink == 1 {
                        // TODO: delete inode
                    }
                    block_cache_sync_all();
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }
    /// Get number of hard links of inode_id
    pub fn get_nlink(&self, inode_id: u32) -> u32 {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut cnt = 0;
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                if dirent == DirEntry::empty() {
                    continue;
                }
                if dirent.inode_id() == inode_id {
                    cnt += 1
                }
            }
            cnt
        })
    }
    /// Get inode id
    pub fn get_inode_id(&self) -> u32 {
        self.inode_id
    }
    /// Whether this inode is a directory
    pub fn is_dir(&self) -> bool {
        self.read_disk_inode(|disk_inode| disk_inode.is_dir())
    }
    /// Whether this inode is a file
    pub fn is_file(&self) -> bool {
        self.read_disk_inode(|disk_inode| disk_inode.is_file())
    }
    /// Read data from current inode
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }
    /// Write data to current inode
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode| {
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }
    /// Clear the data in current inode
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
        block_cache_sync_all();
    }
}