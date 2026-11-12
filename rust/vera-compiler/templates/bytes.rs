/// Logical bounded bytes. Unused storage is canonical zero padding.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
#[repr(C)]
pub struct VeraBytes<const N:usize>{pub len:u64,pub data:[u8;N]}
impl<const N:usize> VeraBytes<N>{
 pub const fn zero()->Self{Self{len:0,data:[0;N]}}
 pub fn from_slice(value:&[u8])->Result<Self,ErrorCode>{if value.len()>N{return Err(ErrorCode::BytesCapacity);}let mut out=Self::zero();out.len=value.len() as u64;out.data[..value.len()].copy_from_slice(value);Ok(out)}
 fn valid(&self)->bool{self.len<=N as u64&&self.data[self.len.min(N as u64) as usize..].iter().all(|b|*b==0)}
 fn logical(&self)->&[u8]{&self.data[..self.len as usize]}
 fn get(&self,index:u64)->Result<u8,ErrorCode>{if index>=self.len{return Err(ErrorCode::BytesBounds);}Ok(self.data[index as usize])}
 fn slice<const M:usize>(&self,start:u64,count:u64)->Result<VeraBytes<M>,ErrorCode>{if start>self.len||count>self.len-start{return Err(ErrorCode::BytesBounds);}VeraBytes::from_slice(&self.data[start as usize..(start+count) as usize])}
 fn resize<const M:usize>(&self)->Result<VeraBytes<M>,ErrorCode>{VeraBytes::from_slice(self.logical())}
 fn concat<const M:usize,const K:usize>(&self,right:&VeraBytes<K>)->Result<VeraBytes<M>,ErrorCode>{let len=self.len.checked_add(right.len).ok_or(ErrorCode::BytesCapacity)?;if len>M as u64{return Err(ErrorCode::BytesCapacity);}let mut out=VeraBytes::zero();out.len=len;out.data[..self.len as usize].copy_from_slice(self.logical());out.data[self.len as usize..len as usize].copy_from_slice(right.logical());Ok(out)}
}
fn vera_encode_bytes<const N:usize>(value:&VeraBytes<N>,out:&mut Vec<u8>){out.extend_from_slice(&value.len.to_le_bytes());out.extend_from_slice(&value.data);}
fn vera_decode_bytes<const N:usize>(r:&mut Reader<'_>)->Result<VeraBytes<N>,ErrorCode>{let value=VeraBytes{len:u64::from_le_bytes(r.take::<8>()?),data:r.take::<N>()?};if !value.valid(){return Err(ErrorCode::InvalidEncoding);}Ok(value)}
