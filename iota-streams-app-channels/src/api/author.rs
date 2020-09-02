use anyhow::{
    bail,
    ensure,
    Result,
};
use core::{
    cell::RefCell,
    fmt::Debug,
};

use iota_streams_core::{
    prelude::{
        hash_map,
        hash_set,
        vec,
        HashMap,
        HashSet,
        Vec,
    },
    prng,
    psk,
    sponge::spongos,
};
use iota_streams_core_edsig::{
    key_exchange::x25519,
    signature::ed25519,
};

use iota_streams_app::message::{
    hdf::{HDF, FLAG_BRANCHING_MASK},
    *,
};
use iota_streams_ddml::types::*;

use super::*;
use crate::message::*;

const ANN_MESSAGE_NUM: usize = 0;
const SEQ_MESSAGE_NUM: usize = 1;

/// Generic Channel Author type parametrised by the type of links, link store and
/// link generator.
///
/// `Link` type defines, well, type of links used by transport layer to identify messages.
/// For example, for HTTP it can be URL, and for the Tangle it's a pair `address`+`tag`
/// transaction fields (see `TangleAddress` type). `Link` type must implement `HasLink`
/// and `AbsorbExternalFallback` traits.
///
/// `Store` type abstracts over different kinds of link storages. Link storage is simply
/// a map from link to a spongos state and associated info corresponding to the message
/// referred by the link. `Store` must implement `LinkStore<Link::Rel>` trait as
/// it's only allowed to link messages within the same channel instance.
///
/// `LinkGen` is a helper tool for deriving links for new messages. It maintains a
/// mutable state and can derive link pseudorandomly.
pub struct AuthorT<F, Link, Store, LinkGen> {
    /// PRNG object used for Ed25519, X25519, Spongos key generation, etc.
    #[allow(dead_code)]
    prng: prng::Prng<F>,

    /// Own Ed25519 private key.
    pub(crate) sig_kp: ed25519::Keypair,

    /// Own x25519 key pair corresponding to Ed25519 keypair.
    pub(crate) ke_kp: (x25519::StaticSecret, x25519::PublicKey),

    /// Combined public key and nonce value used for app instance link generation
    channel_addr: Vec<u8>,

    /// Subscribers' pre-shared keys.
    pub psks: psk::Psks,

    /// Subscribers' trusted X25519 public keys.
    pub ke_pks: x25519::Pks,

    /// Link store.
    store: RefCell<Store>,

    /// Link generator.
    pub(crate) link_gen: LinkGen,

    /// Link to the announce message, ie. application instance.
    pub(crate) appinst: Link,

    /// u8 indicating if flags is used (0 = false, 1 = true)
    pub flags: u8,

    /// Mapping of publisher id to sequence state
    pub(crate) seq_states: HashMap<Vec<u8>, (Link, usize)>,

    pub message_encoding: Vec<u8>,

    pub uniform_payload_length: usize,


}

impl<F, Link, Store, LinkGen> AuthorT<F, Link, Store, LinkGen>
where
    F: PRP,
    Link: HasLink + AbsorbExternalFallback<F>,
    <Link as HasLink>::Base: Eq + Debug,
    <Link as HasLink>::Rel: Eq + Debug + SkipFallback<F>,
    Store: LinkStore<F, <Link as HasLink>::Rel>,
    LinkGen: ChannelLinkGenerator<Link>,
{
    /// Create a new Author and generate MSS and optionally NTRU key pair.
    pub fn gen(
        store: Store,
        mut link_gen: LinkGen,
        prng: prng::Prng<F>,
        nonce: Vec<u8>,
        flags: u8,
        message_encoding: Vec<u8>,
        uniform_payload_length: usize,
    ) -> Self {
        let sig_kp = ed25519::Keypair::generate(&mut prng::Rng::new(prng.clone(), nonce.clone()));
        let ke_kp = x25519::keypair_from_ed25519(&sig_kp);

        // App instance link is generated using the 32 byte PubKey and the first 8 bytes of the nonce
        let mut appinst_input = Vec::new();
        appinst_input.extend_from_slice(&sig_kp.public.to_bytes()[..]);
        appinst_input.extend_from_slice(&nonce[0..8]);

        let appinst = link_gen.link_from(&(appinst_input.clone(), ke_kp.1, ANN_MESSAGE_NUM));

        // Start sequence state of new publishers to 2
        // 0 is used for Announce/Subscribe/Unsubscribe
        // 1 is used for sequence messages
        let mut seq_map = HashMap::new();
        seq_map.insert(ke_kp.1.as_bytes().to_vec(), (appinst.clone(), 2 as usize));

        Self {
            prng: prng,
            sig_kp: sig_kp,
            ke_kp: ke_kp,

            psks: HashMap::new(),
            ke_pks: HashSet::new(),

            store: RefCell::new(store),
            link_gen: link_gen,
            appinst: appinst,
            channel_addr: appinst_input,
            flags: flags,
            seq_states: seq_map,
            message_encoding: message_encoding,
            uniform_payload_length: uniform_payload_length,
        }
    }

    /// Prepare Announcement message.
    pub fn prepare_announcement<'a>(
        &'a mut self,
    ) -> Result<PreparedMessage<'a, F, Link, Store, announce::ContentWrap<F>>> {
        // Create HDF for the first message in the channel.
        let header =
            self.link_gen.header_from(&(self.channel_addr.clone(), (self.ke_kp.1).clone(), ANN_MESSAGE_NUM),
                                        ANNOUNCE,
                                      1,
            );
        let content = announce::ContentWrap::new(&self.sig_kp, self.flags);
        Ok(PreparedMessage::new(self.store.borrow(), header, content))
    }

    /// Create Announce message.
    pub fn announce<'a>(
        &'a mut self,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<BinaryMessage<F, Link>> {
        let wrapped = self.prepare_announcement()?.wrap()?;
        let r = wrapped.commit(self.store.borrow_mut(), info);
        r
    }

    fn do_prepare_keyload<'a, Psks, KePks>(
        &'a self,
        header: HDF<Link>,
        link_to: &'a <Link as HasLink>::Rel,
        psks: Psks,
        ke_pks: KePks,
    ) -> Result<PreparedMessage<'a, F, Link, Store, keyload::ContentWrap<'a, F, Link, Psks, KePks>>>
    where
        Psks: Clone + ExactSizeIterator<Item = psk::IPsk<'a>>,
        KePks: Clone + ExactSizeIterator<Item = x25519::IPk<'a>>,
    {
        let nonce = NBytes(prng::random_nonce(spongos::Spongos::<F>::NONCE_SIZE));
        let key = NBytes(prng::random_key(spongos::Spongos::<F>::KEY_SIZE));
        let content = keyload::ContentWrap {
            link: link_to,
            nonce: nonce,
            key: key,
            psks: psks,
            ke_pks: ke_pks,
            _phantom: core::marker::PhantomData,
        };
        Ok(PreparedMessage::new(self.store.borrow(), header, content))
    }

    pub fn prepare_keyload<'a>(
        &'a mut self,
        link_to: &'a <Link as HasLink>::Rel,
        psk_ids: &psk::PskIds,
        ke_pks: &'a Vec<x25519::PublicKeyWrap>,
    ) -> Result<
        PreparedMessage<
            'a,
            F,
            Link,
            Store,
            keyload::ContentWrap<'a, F, Link, vec::IntoIter<psk::IPsk<'a>>, vec::IntoIter<x25519::IPk<'a>>>,
        >,
    > {
        let header = self.link_gen.header_from(
            &(link_to.clone(), (self.ke_kp.1).clone(), self.get_seq_num()),
            KEYLOAD,
            1
        );
        let psks = psk::filter_psks(&self.psks, psk_ids);
        let ke_pks = x25519::filter_ke_pks(&self.ke_pks, ke_pks);
        self.do_prepare_keyload(header, link_to, psks.into_iter(), ke_pks.into_iter())
    }

    pub fn prepare_keyload_for_everyone<'a>(
        &'a mut self,
        link_to: &'a <Link as HasLink>::Rel,
    ) -> Result<
        PreparedMessage<
            'a,
            F,
            Link,
            Store,
            keyload::ContentWrap<
                'a,
                F,
                Link,
                hash_map::Iter<psk::PskId, psk::Psk>,
                hash_set::Iter<x25519::PublicKeyWrap>,
            >,
        >,
    > {
        let header = self.link_gen.header_from(
            &(link_to.clone(), self.ke_kp.1.clone(), self.get_seq_num()),
            KEYLOAD,
        1
        );
        let ipsks = self.psks.iter();
        let ike_pks = self.ke_pks.iter();
        self.do_prepare_keyload(header, link_to, ipsks, ike_pks)
    }

    /// Create keyload message with a new session key shared with recipients
    /// identified by pre-shared key IDs and by NTRU public key IDs.
    pub fn share_keyload(
        &mut self,
        link_to: &<Link as HasLink>::Rel,
        psk_ids: &psk::PskIds,
        ke_pks: &Vec<x25519::PublicKeyWrap>,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<BinaryMessage<F, Link>> {
        let wrapped = self.prepare_keyload(link_to, psk_ids, ke_pks)?.wrap()?;
        wrapped.commit(self.store.borrow_mut(), info)
    }

    /// Create keyload message with a new session key shared with all Subscribers
    /// known to Author.
    pub fn share_keyload_for_everyone(
        &mut self,
        link_to: &<Link as HasLink>::Rel,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<BinaryMessage<F, Link>> {
        let wrapped = self.prepare_keyload_for_everyone(link_to)?.wrap()?;
        wrapped.commit(self.store.borrow_mut(), info)
    }

    pub fn prepare_sequence<'a>(
        &'a mut self,
        link_to: &'a <Link as HasLink>::Rel,
        seq_num: usize,
        ref_link: NBytes,
    ) -> Result<PreparedMessage<'a, F, Link, Store, sequence::ContentWrap<'a, Link>>> {
        let header = self.link_gen.header_from(
            &(link_to.clone(), self.ke_kp.1.clone(), SEQ_MESSAGE_NUM),
            SEQUENCE,
            1);

        let content = sequence::ContentWrap {
            link: link_to,
            pubkey: &self.ke_kp.1,
            seq_num: seq_num,
            ref_link: ref_link,
        };

        Ok(PreparedMessage::new(self.store.borrow(), header, content))
    }

    /// Send sequence message to show referenced message
    pub fn sequence<'a>(
        &mut self,
        ref_link: Vec<u8>,
        seq_link: <Link as HasLink>::Rel,
        seq_num: usize,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<BinaryMessage<F, Link>> {
        let wrapped = self.prepare_sequence(&seq_link, seq_num, NBytes(ref_link))?.wrap()?;

        wrapped.commit(self.store.borrow_mut(), info)
    }

    /// Prepare SignedPacket message.
    pub fn prepare_signed_packet<'a>(
        &'a mut self,
        link_to: &'a <Link as HasLink>::Rel,
        public_payload: &'a Bytes,
        masked_payload: &'a Bytes,
    ) -> Result<PreparedMessage<'a, F, Link, Store, signed_packet::ContentWrap<'a, F, Link>>> {
        let header = self.link_gen.header_from(
            &(link_to.clone(), self.ke_kp.1.clone(), self.get_seq_num()),
            SIGNED_PACKET,
            1);
        let content = signed_packet::ContentWrap {
            link: link_to,
            public_payload: public_payload,
            masked_payload: masked_payload,
            sig_kp: &self.sig_kp,
            _phantom: core::marker::PhantomData,
        };
        Ok(PreparedMessage::new(self.store.borrow(), header, content))
    }

    /// Create a signed message with public and masked payload.
    pub fn sign_packet(
        &mut self,
        link_to: &<Link as HasLink>::Rel,
        public_payload: &Bytes,
        masked_payload: &Bytes,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<BinaryMessage<F, Link>> {
        let wrapped = self
            .prepare_signed_packet(link_to, public_payload, masked_payload)?
            .wrap()?;
        wrapped.commit(self.store.borrow_mut(), info)
    }

    /// Prepare TaggedPacket message.
    pub fn prepare_tagged_packet<'a>(
        &'a mut self,
        link_to: &'a <Link as HasLink>::Rel,
        public_payload: &'a Bytes,
        masked_payload: &'a Bytes,
    ) -> Result<PreparedMessage<'a, F, Link, Store, tagged_packet::ContentWrap<'a, F, Link>>> {
        let header = self.link_gen.header_from(
            &(link_to.clone(), self.ke_kp.1.clone(), self.get_seq_num()),
            TAGGED_PACKET,
            1);
        let content = tagged_packet::ContentWrap {
            link: link_to,
            public_payload: public_payload,
            masked_payload: masked_payload,
            _phantom: core::marker::PhantomData,
        };
        Ok(PreparedMessage::new(self.store.borrow(), header, content))
    }

    /// Create a tagged (ie. MACed) message with public and masked payload.
    /// Tagged messages must be linked to a secret spongos state, ie. keyload or a message linked to keyload.
    pub fn tag_packet(
        &mut self,
        link_to: &<Link as HasLink>::Rel,
        public_payload: &Bytes,
        masked_payload: &Bytes,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<BinaryMessage<F, Link>> {
        let wrapped = self
            .prepare_tagged_packet(link_to, public_payload, masked_payload)?
            .wrap()?;
        wrapped.commit(self.store.borrow_mut(), info)
    }

    fn ensure_appinst<'a>(&self, preparsed: &PreparsedMessage<'a, F, Link>) -> Result<()> {
        ensure!(
            self.appinst.base() == preparsed.header.link.base(),
            "Message sent to another channel instance."
        );
        Ok(())
    }

    fn lookup_psk<'b>(&'b self, pskid: &psk::PskId) -> Option<&'b psk::Psk> {
        self.psks.get(pskid)
    }

    fn lookup_ke_sk<'b>(&'b self, ke_pk: &x25519::PublicKey) -> Option<&'b x25519::StaticSecret> {
        if (self.ke_kp.1).as_bytes() == ke_pk.as_bytes() {
            Some(&self.ke_kp.0)
        } else {
            None
        }
    }

    pub fn unwrap_keyload<'a, 'b>(
        &'b self,
        preparsed: PreparsedMessage<'a, F, Link>,
    ) -> Result<
        UnwrappedMessage<
            F,
            Link,
            keyload::ContentUnwrap<
                'b,
                F,
                Link,
                Self,
                for<'c> fn(&'c Self, &psk::PskId) -> Option<&'c psk::Psk>,
                for<'c> fn(&'c Self, &x25519::PublicKey) -> Option<&'c x25519::StaticSecret>,
            >,
        >,
    > {
        self.ensure_appinst(&preparsed)?;
        let content = keyload::ContentUnwrap::<
            'b,
            F,
            Link,
            Self,
            for<'c> fn(&'c Self, &psk::PskId) -> Option<&'c psk::Psk>,
            for<'c> fn(&'c Self, &x25519::PublicKey) -> Option<&'c x25519::StaticSecret>,
        >::new(self, Self::lookup_psk, Self::lookup_ke_sk);
        preparsed.unwrap(&*self.store.borrow(), content)
    }

    /// Try unwrapping session key from keyload using Subscriber's pre-shared key or NTRU private key (if any).
    pub fn handle_keyload<'a>(
        &mut self,
        preparsed: PreparsedMessage<'a, F, Link>,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<()> {
        let _content = self.unwrap_keyload(preparsed)?.commit(self.store.borrow_mut(), info)?;
        // Unwrapped nonce and key in content are not used explicitly.
        // The resulting spongos state is joined into a protected message state.
        Ok(())
    }

    pub fn unwrap_tagged_packet<'a>(
        &self,
        preparsed: PreparsedMessage<'a, F, Link>,
    ) -> Result<UnwrappedMessage<F, Link, tagged_packet::ContentUnwrap<F, Link>>> {
        self.ensure_appinst(&preparsed)?;
        let content = tagged_packet::ContentUnwrap::new();
        preparsed.unwrap(&*self.store.borrow(), content)
    }

    /// Get public payload, decrypt masked payload and verify MAC.
    pub fn handle_tagged_packet<'a>(
        &mut self,
        preparsed: PreparsedMessage<'a, F, Link>,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<(Bytes, Bytes)> {
        let content = self
            .unwrap_tagged_packet(preparsed)?
            .commit(self.store.borrow_mut(), info)?;
        Ok((content.public_payload, content.masked_payload))
    }

    pub fn unwrap_subscribe<'a>(
        &self,
        preparsed: PreparsedMessage<'a, F, Link>,
    ) -> Result<UnwrappedMessage<F, Link, subscribe::ContentUnwrap<F, Link>>> {
        self.ensure_appinst(&preparsed)?;
        let content = subscribe::ContentUnwrap::new(&self.ke_kp.0);
        preparsed.unwrap(&*self.store.borrow(), content)
    }

    /// Get public payload, decrypt masked payload and verify MAC.
    pub fn handle_subscribe<'a>(
        &mut self,
        preparsed: PreparsedMessage<'a, F, Link>,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<()> {
        let content = self
            .unwrap_subscribe(preparsed)?
            .commit(self.store.borrow_mut(), info)?;
        // TODO: trust content.subscriber_ntru_pk and add to the list of subscribers only if trusted.
        let subscriber_sig_pk = content.subscriber_sig_pk;
        let subscriber_ke_pk = x25519::public_from_ed25519(&subscriber_sig_pk);
        self.ke_pks.insert(x25519::PublicKeyWrap(subscriber_ke_pk));
        self.store_state(subscriber_ke_pk, self.appinst.clone(), SEQ_MESSAGE_NUM);
        // Unwrapped unsubscribe_key is not used explicitly.
        Ok(())
    }

    pub fn unwrap_sequence<'a>(
        &self,
        preparsed: PreparsedMessage<'a, F, Link>,
    ) -> Result<UnwrappedMessage<F, Link, sequence::ContentUnwrap<Link>>> {
        self.ensure_appinst(&preparsed)?;
        let content = sequence::ContentUnwrap::default();
        preparsed.unwrap(&*self.store.borrow(), content)
    }

    // Fetch unwrapped sequence message to fetch referenced message
    pub fn handle_sequence<'a>(
        &mut self,
        preparsed: PreparsedMessage<'a, F, Link>,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<sequence::ContentUnwrap<Link>> {
        let content = self.unwrap_sequence(preparsed)?.commit(self.store.borrow_mut(), info)?;
        Ok(content)
    }

    // pub fn unwrap_unsubscribe<'a>(
    // &self,
    // preparsed: PreparsedMessage<'a, F, Link>,
    // ) -> Result<UnwrappedMessage<F, Link, unsubscribe::ContentUnwrap<F, Link>>> {
    // self.ensure_appinst(&preparsed)?;
    // let content = unsubscribe::ContentUnwrap::new();
    // preparsed.unwrap(&*self.store.borrow(), content)
    // }
    //
    // Get public payload, decrypt masked payload and verify MAC.
    // pub fn handle_unsubscribe<'a>(
    // &mut self,
    // preparsed: PreparsedMessage<'a, F, Link>,
    // info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    // ) -> Result<()> {
    // let _content = self
    // .unwrap_unsubscribe(preparsed)?
    // .commit(self.store.borrow_mut(), info)?;
    // Ok(())
    // }

    /// Unwrap message with default logic.
    pub fn handle_msg(
        &mut self,
        msg: &BinaryMessage<F, Link>,
        info: <Store as LinkStore<F, <Link as HasLink>::Rel>>::Info,
    ) -> Result<()> {
        let preparsed = msg.parse_header()?;
        self.ensure_appinst(&preparsed)?;

        if preparsed.check_content_type(&TAGGED_PACKET) {
            self.handle_tagged_packet(preparsed, info)?;
            Ok(())
        } else if preparsed.check_content_type(&ANNOUNCE) {
            bail!("Can't handle announce message.")
        } else if preparsed.check_content_type(&SIGNED_PACKET) {
            bail!("Can't handle signed_packet message.")
        } else {
            bail!("Unsupported content type: '{}'.", preparsed.content_type())
        }
    }

    pub fn get_branching_flag(&self) -> u8 {
        self.flags & FLAG_BRANCHING_MASK
    }

    pub fn gen_msg_id(&mut self, link: &<Link as HasLink>::Rel, pk: x25519::PublicKey, seq: usize) -> Link {
        self.link_gen.link_from(&(link.clone(), pk, seq))
    }

    pub fn get_pks(&self) -> x25519::Pks {
        self.ke_pks.clone()
    }

    /// Store the sequence state of a given publisher
    pub fn store_state(&mut self, pubkey: x25519::PublicKey, msg_link: Link, seq_num: usize) {
        self.seq_states.insert(pubkey.as_bytes().to_vec(), (msg_link, seq_num));
    }

    /// Retrieve the sequence state for a given publisher
    pub fn get_seq_state(&self, pubkey: x25519::PublicKey) -> Option<(Link, usize)> {
        self.seq_states.get(&pubkey.as_bytes().to_vec()).cloned()
    }

    pub fn get_seq_num(&self) -> usize {
        self.get_seq_state(self.ke_kp.1).unwrap().1
    }
}