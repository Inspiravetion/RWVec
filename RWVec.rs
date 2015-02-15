
/*
  TODO: 
    Guards: 
        Implement - every trait there deref data type does
 */

//send, copy, and sync?
#[derive(Clone)]
struct RWVec<T> {
    rw_lock   : Box<StaticRwLock>,
    push_lock : Box<StaticMutex>,
    data      : UnsafeCell<std::vec::Vec<T>>
}

impl<T> RWVec<T> {
    pub fn new() -> RWVec<T> {
        Vec {  
            rw_lock   : box RWLOCK_INIT,
            push_lock : box MUTEX_INIT,
            data      : UnsafeCell::new(std::vec::Vec::new())
        }
    }

    pub fn push(&mut self, t : T) {
        //compete with other pushers
        unsafe { self.push_lock.lock.lock(); }
        
        //the push will cause a realloc
        if self.data.get().cap() == self.data.get().len() {
            //compete with other pushers and all the readers as well
            unsafe { self.rw_lock.lock.write(); }
            //push reallocs underlying mem and copys over old values
            self.data.get().push(t);

            unsafe { 
                //safe to read
                self.rw_lock.lock.write_unlock();
                //safe to push again
                self.push_lock.lock.unlock();
            }

            return
        }
        
        //push that doesnt affect reads
        self.data.get().push(t);
        //safe to push again
        unsafe { self.push_lock.lock.unlock(); }
    }

    pub fn reader(&self) -> SliceGuard<T> {
        //return a view of the current snapshot 
        SliceGuard::new(self.data.get()[..], &self.rwlock)
    }
    
    pub fn writer(&mut self) -> SliceGuardMut<T> {
        //return a mutable, upgradable view of the current snapshot 
        SliceGuardMut::new(self.data.get()[..], &self.rwlock, &self.push_lock)
    }
}

impl<T> Drop for RWVec<T> {
    fn drop(&mut self) {
        unsafe { self.rw_lock.lock.destroy() }
        unsafe { self.push_lock.lock.destroy() }
    }
}

/*
    IMMUTABLE GUARD
*/

//multiple read access to a slice representing the current
//state of the Vec...pushers can still push on the vec as long as they don't 
//need to reallocate
struct SliceGuard<'locked, T> {
    //the underlying vec
    vec         : &'locked std::vec::Vec<T>
    //how far to slice on deref...pushers may have corrupted past here
    end         : uint,
    //unlock on drop
    resize_lock : &'locked Box<StaticRWLock>,
    //in case we need to upgrade this needs to be accuired
    push_lock   : &'locked Box<StaticMutex>
}   

impl<'locked, T> SliceGuard<'locked, T> {
    fn new(vec : &std::vec::Vec<T>, resize_lock :  &Box<StaticRWLock>, push_lock : &Box<StaticMutex>) -> SliceGuard<'locked, T> {
        unsafe { resize_lock.lock.read() }

        struct SliceGuard {
            vec         : vec
            end         : 0,
            resize_lock : resize_lock,
            push_lock   : push_lock
        }   
    }

    //this updates your view of the vec by yielding and then acquiring both locks
    fn refresh(&mut self) { 
        unsafe {
            //give the pending reallocating pushers a chance to finish so no deadlock
            self.resize_lock.lock.read_unlock(); 
            //seal off the pushers
            self.push_lock.lock.lock();
            //register yourself as a reader again
            self.resize_lock.lock.read(); 
        }

        self.end = vec.len();

        unsafe {
            //let non-reallocating pushers in again
            self.push_lock.unlock();
        } 
    }
}

impl<'locked, T> IntoIterator for &'locked SliceGuard<T> {
    type A = &'locked T;
    type I = std::slice::Iter<'locked, T>;

    fn into_iter(self) -> I {
        //the deref on the functin call delegates this to the slice
        self.iter()
    }
}

impl<'locked, T> Deref for SliceGuard<'locked, T> {
    type Target = [T];

    fn deref<'a>(&'a self) -> &'a Target {
        self.vec[..self.end]
    }
}

#[unsafe_destructor]
impl<T> Drop for SliceGuard<T> { 
    fn drop(&mut self) {
        self.resize_lock.lock.read_unlock();
    }
}

/*
    MUTABLE GUARDS
*/

//Exlusive read and write access to a slice representing the current
//state of the Vec...pushers can still push on the vec as long as they don't 
//need to reallocate
struct SliceGuardMut<'locked, T> {
    //the underlying vec
    vec         : &'locked std::vec::Vec<T>
    //how far to slice on deref...pushers may have corrupted past here
    end         : uint,
    //unlock on drop
    resize_lock : &'locked Box<StaticRWLock>,
    //in case we need to upgrade this needs to be accuired
    push_lock   : &'locked Box<StaticMutex>
}   

impl<T> SliceGuardMut<T> {
    fn new(vec: &'locked std::vec::Vec<T>, resize_lock : &'locked Box<StaticRWLock>, push_lock : &'locked Box<StaticMutex>) -> SliceGuardMut<'locked, T> {
        unsafe { resize_lock.lock.write() }

        SliceGuardMut {
            //the underlying vec
            vec         : vec
            //how far to slice on deref...pushers may have corrupted past here
            end         : vec.len(),
            //unlock on drop
            resize_lock : resize_lock,
            //in case we need to upgrade this needs to be accuired
            push_lock   : push_lock
        }   
    }

    //this updates your view of the vec by yielding and then acquiring both locks
    fn refresh(&mut self) { 
        unsafe {
            //release pushers waiting to realloc
            self.resize_lock.lock.write_unlock();

            //seal off pushers
            self.push_lock.lock.lock();

            //wait for immutable readers to be dropped then lock out new ones
            self.resize_lock.lock.write();
        }

        self.end = vec.len();

        unsafe {
            //let non-reallocating pushers in again
            self.push_lock.lock.unlock();
        } 
    }

    //this acquires the push lock as well so you have exclusive access
    //this is basically a scoped version of refresh that lets you exclusively mutate the whole vec 
    //until the guard drops
    fn upgrade(&self) -> VecGuardMut<T> { 
        unsafe {
            //give the pending reallocating pushers a chance to finish so no deadlock
            self.resize_lock.lock.write_unlock(); 
            //seal off the pushers by creating a vec guard
            let vec_guard = VecGuardMut::new(self.push_lock);
            //seal off any other reader
            self.resize_lock.lock.write(); 

            vec_guard
        }
    }
}

impl<'locked, T> IntoIterator for &'locked SliceGuardMut<T> {
    type A = &'locked T;
    type I = std::slice::Iter<'locked, T>;

    fn into_iter(self) -> I {
        //the deref on the functin call delegates this to the slice
        self.iter()
    }
}

impl<'locked, T> IntoIterator for &'locked mut SliceGuardMut<T> {
    type A = &'locked mut T;
    type I = std::slice::IterMut<'locked, T>;

    fn into_iter(self) -> I {
        //the deref on the functin call delegates this to the slice
        self.iter()
    }
}

impl<'locked, T> Deref for SliceGuardMut<'locked, T> {
    type Target = [T];

    fn deref<'a>(&'a self) -> &'a Target {
        self.vec[..self.end]
    }
}

impl<'locked, T> DerefMut for SliceGuardMut<'locked, T> {
    type Target = [T];

    fn deref<'a>(&'a mut self) -> &'a mut Target {
        self.vec[..self.end]
    }
}

#[unsafe_destructor]
impl<T> Drop for SliceGuardMut<T> { 
    fn drop(&mut self) {
        unsafe { self.resize_lock.lock.write_unlock(); }
    }
}

//Exclusive read and write acces to the whole vec...pushers get blocked while
//they wait for this to drop
struct VecGuardMut<'locked, T> {
    //exclusive access to the vec
    vec    : &'locked std::vec::Vec<T>,
    //unlock this on drop 
    lock   : &'locked Box<StaticMutex>
}

impl<'locked, T> VecGuardMut<'locked, T> {
    fn new(vec : &'locked std::vec::Vec<T>, push_lock : &'locked Box<StaticMutex>) -> VecGuardMut<'locked, T> {
        unsafe { push_lock.lock.lock() }

        VecGuardMut {
            vec : vec,
            lock : push_lock
        }
    }
}

impl<'locked, T> IntoIterator for &'locked VecGuardMut<T> {
    type A = &'locked T;
    type I = std::slice::Iter<'locked, T>;

    fn into_iter(self) -> I {
        //the deref on the functin call delegates this to the vec
        self.into_iter()
    }
}

impl<'locked, T> IntoIterator for &'locked mut VecGuardMut<T> {
    type A = &'locked mut T;
    type I = std::slice::IterMut<'locked, T>;

    fn into_iter(self) -> I {
        //the deref on the functin call delegates this to the vec
        self.into_iter()
    }
}

impl<'locked, T> Deref for VecGuardMut<'locked, T> {
    type Target = std::vec::Vec<T>;

    fn deref<'a>(&'a self) -> &'a Target {
        self.vec
    }
}


impl<'locked, T> DerefMut for VecGuardMut<'locked, T> {
    type Target = std::vec::Vec<T>;

    fn deref_mut<'a>(&mut self) -> &'a mut Target {
        self.vec
    }
}

#[unsafe_destructor]
impl<T> Drop for VecGuardMut<T> {
    fn drop(&mut self) {
        self.lock.lock.unlock();
    }
}