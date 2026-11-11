struct Reader<'a> { data:&'a [u8], pos:usize }
impl<'a> Reader<'a> {
    fn new(data:&'a [u8])->Self {Self{data,pos:0}}
    fn take<const N:usize>(&mut self)->Result<[u8;N],ErrorCode>{let end=self.pos.checked_add(N).ok_or(ErrorCode::InvalidEncoding)?;let b=self.data.get(self.pos..end).ok_or(ErrorCode::InvalidEncoding)?;self.pos=end;Ok(b.try_into().unwrap())}
    fn boolean(&mut self)->Result<bool,ErrorCode>{match self.take::<1>()?[0]{0=>Ok(false),1=>Ok(true),_=>Err(ErrorCode::InvalidEncoding)}}
    fn done(&self)->Result<(),ErrorCode>{if self.pos==self.data.len(){Ok(())}else{Err(ErrorCode::InvalidEncoding)}}
}
fn mix64(z:u64)->u64{let z=z^(z>>30);let z=z.wrapping_mul(0xBF58476D1CE4E5B9);let z=z^(z>>27);let z=z.wrapping_mul(0x94D049BB133111EB);z^(z>>31)}
